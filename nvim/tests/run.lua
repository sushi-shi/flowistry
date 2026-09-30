local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
vim.cmd("runtime plugin/flowistry.lua")
local flow = require("flowistry")
local ranges = require("flowistry.ranges")
local render = require("flowistry.render")
local backend = require("flowistry.backend")
local temp = vim.fn.tempname() .. " flowistry project"
vim.fn.mkdir(temp .. "/src", "p")
vim.fn.writefile({ '[package]', 'name = "fixture"', 'version = "0.1.0"', 'edition = "2021"' }, temp .. "/Cargo.toml")
local source = {
  "fn main() {", "    let x = 1;", "    let y = 2;", '    println!("{}", x); ',
  '    println!("{}", y); ', "}", "", "fn outer() {", "    fn inner() {", "        let z = 3;", "    }", "}",
}
local file = temp .. "/src/main.rs"
vim.fn.writefile(source, file)
vim.cmd.edit(vim.fn.fnameescape(file))
vim.bo.filetype = "rust"
local buf = vim.api.nvim_get_current_buf()
local notes = {}
vim.notify = function(message) notes[#notes + 1] = message end
local log = temp .. "/requests.jsonl"
local command = { vim.v.progpath, "--headless", "-u", "NONE", "-l", repo .. "/tests/fake_backend.lua" }
local passed = 0
local function check(condition, message) assert(condition, message); passed = passed + 1 end
local function equal(a, b, message) check(vim.deep_equal(a, b), message .. "\n" .. vim.inspect(a) .. " ~= " .. vim.inspect(b)) end
local function await(fn, message) check(vim.wait(6000, fn, 10), message) end
local function setup(env)
  flow.setup({ command = command, auto_enable = false, debounce_ms = 5,
    env = vim.tbl_extend("force", { FLOWISTRY_TEST_LOG = log }, env or {}) })
end
local function move(row, column)
  vim.api.nvim_win_set_cursor(0, { row, column })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = buf })
end
local function marks() return vim.api.nvim_buf_get_extmarks(buf, render.namespace, 0, -1, { details = true }) end
local function calls()
  local result = {}
  for _, line in ipairs(vim.fn.readfile(log)) do result[#result + 1] = vim.json.decode(line) end
  return result
end
local function range(a, b) return { start = { 0, a }, finish = { 0, b } } end

local function run()
  equal(ranges.complement({ range(0, 20) }, { range(2, 8), range(5, 12), range(17, 30) }),
    { range(0, 2), range(12, 17) }, "overlapping slices clip to containers")
  equal(ranges.complement({ range(5, 15) }, { range(0, 20) }), {}, "covering slice leaves no dimming")
  equal(ranges.complement({ range(5, 15) }, {}), { range(5, 15) }, "empty slice dims full container")
  check(not ranges.contains(range(0, 5), { 0, 5 }), "ranges are half-open")
  local inner = { range = range(4, 6) }
  equal(ranges.smallest({ { range = range(0, 20) }, inner }, { 0, 5 }, function(v) return v.range end), inner,
    "smallest nested function wins")

  local unicode = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(unicode, temp .. "/unicode.rs")
  vim.api.nvim_buf_set_lines(unicode, 0, -1, false, { "aé🦀éx" })
  local convert = ranges.converter(unicode, temp)
  local span = { filename = "unicode.rs", start = { line = 0, column = 2 }, ["end"] = { line = 0, column = 5 } }
  equal(convert(span), range(3, 10), "Unicode scalars become byte columns, including combining marks")
  equal(ranges.position(unicode, { 1, 10 }), { 0, 5 }, "cursor bytes become Unicode scalars")
  local indexed = ranges.converter(unicode, temp, 12)
  span.filename = 12
  equal(indexed(span), range(3, 10), "pinned backend's numeric file identity is supported")
  span.filename = 13
  equal(indexed(span), nil, "foreign numeric file identities are ignored")
  span.filename = "elsewhere.rs"
  equal(convert(span), nil, "foreign file spans ignored")
  span.filename, span["end"].column = "unicode.rs", 999
  check(not pcall(convert, span), "out-of-bounds analysis rejected")
  vim.api.nvim_buf_delete(unicode, { force = true })

  local syntax_buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(syntax_buf, 0, -1, false, {
    "fn restore(map: &LevelMap) {", "  // travel comment", "  let state = map;",
    "  /* nested", "     comment */ state;", "}",
  })
  local binding = { start = { 0, 11 }, finish = { 0, 14 } }
  local argument_type = { start = { 0, 16 }, finish = { 0, 25 } }
  local whole = { start = { 0, 0 }, finish = { 5, 1 } }
  local comments = {
    { start = { 1, 2 }, finish = { 1, 19 } },
    { start = { 3, 2 }, finish = { 4, 15 } },
  }
  local syntax_focus = {
    containers = { whole }, comments = comments,
    parameter_aliases = { { range = argument_type, target = binding } },
    places = { { range = binding, ranges = { binding }, slice = { whole },
      maybe_slice = { whole }, direct_influence = { whole } } },
  }
  local binding_slice = render.show(syntax_buf, syntax_focus, binding.start, 200, true)
  for _, column in ipairs({ 16, 17, 22, 24 }) do
    equal(render.show(syntax_buf, syntax_focus, { 0, column }, 200, true), binding_slice,
      "type selection, including &, has the binding's slice")
  end
  equal(render.show(syntax_buf, syntax_focus, { 0, 18 }, 200, true, true, false), nil,
    "parameter_types=false disables the alias")
  for _, slice in ipairs({ { whole }, { binding } }) do
    syntax_focus.places[1].slice = slice
    render.show(syntax_buf, syntax_focus, binding.start, 200, true)
    for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(syntax_buf, render.namespace, 0, -1, { details = true })) do
      local marked = { start = { mark[2], mark[3] }, finish = { mark[4].end_row, mark[4].end_col } }
      for _, comment in ipairs(comments) do
        check(not (ranges.before(marked.start, comment.finish) and ranges.before(comment.start, marked.finish)),
          "no dim, focus, influence or maybe decoration overlaps comments")
      end
    end
  end
  equal(render.show(syntax_buf, syntax_focus, { 1, 6 }, 200, true), nil, "a comment word cannot select a place")
  equal(#vim.api.nvim_buf_get_extmarks(syntax_buf, render.namespace, 0, -1, {}), 0,
    "entering a comment clears previous focus decorations")
  vim.api.nvim_buf_delete(syntax_buf, { force = true })

  local pins = require("flowistry.pin")
  local anchor_buf = vim.api.nvim_create_buf(false, true)
  local original_lines = { "fn main() {", "    let dx = end[0] - start[0];",
    "    let dy = end[1] - start[1];", "    consume(dx, dy);", "}" }
  local function anchor_case(lines, expected, message, initial)
    vim.api.nvim_buf_set_lines(anchor_buf, 0, -1, false, initial or original_lines)
    local pin = pins.set(anchor_buf, { 2, 9 }) -- Second byte of dy.
    vim.api.nvim_buf_set_lines(anchor_buf, 0, -1, false, lines)
    equal(pins.position(anchor_buf, pin), expected, message)
    pins.clear(anchor_buf)
  end
  anchor_case(original_lines, { 2, 9 }, "identical whole-buffer replacement preserves pinned column")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];", "",
    "    let dy = end[1] - start[1];", "    consume(dx, dy);", "}" }, { 3, 9 },
    "blank-line insertion followed by replacement preserves pin")
  anchor_case({ "// header", "fn main() {", "    let dx = end[0] - start[0];",
    "", "    let dy = end[1] - start[1];", "    consume(dx, dy);", "}" }, { 4, 9 },
    "multiple insertions before the pin move it correctly")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];",
    "        let dy=end[1]-start[1];", "    consume(dx, dy);", "}" }, { 2, 13 },
    "indentation and spacing changes preserve pinned identifier")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];",
    "    let", "        dy = end[1] - start[1];", "    consume(dx, dy);", "}" }, { 3, 9 },
    "formatter line splitting preserves pinned identifier")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];",
    "    let dx = end[1] - start[1];", "    consume(dx, dy);", "}" }, nil,
    "a changed target does not silently select another variable")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];",
    "    let dy_changed = end[1] - start[1];", "    consume(dx, dy);", "}" }, nil,
    "extending a name is recognized as a changed target")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];",
    "    consume(dx, dy);", "}" }, nil, "deleting the target does not retarget another occurrence")
  anchor_case({ "fn main() {", "    let dx = end[0] - start[0];",
    "    let café = 3;", "}" }, { 2, 12 }, "UTF-8 pin columns survive replacement",
    { "fn main() {", "    let dx = end[0] - start[0];", " let café = 3;", "}" })
  -- A missing target remains pinned, and can recover when an edit is undone.
  vim.api.nvim_buf_set_lines(anchor_buf, 0, -1, false, original_lines)
  local lost_pin = pins.set(anchor_buf, { 2, 8 })
  check(pins.contains(anchor_buf, lost_pin, { 2, 9 }), "same pin matches another character of the variable")
  check(not pins.contains(anchor_buf, lost_pin, { 2, 10 }), "same-pin match excludes following whitespace")
  vim.api.nvim_buf_set_lines(anchor_buf, 0, 0, false, { "// inserted" })
  check(pins.contains(anchor_buf, lost_pin, { 3, 9 }), "same-pin match follows unsaved line insertion")
  vim.api.nvim_buf_set_lines(anchor_buf, 0, -1, false, original_lines)
  check(pins.contains(anchor_buf, lost_pin, { 2, 8 }), "same-pin match follows restored source")
  vim.api.nvim_buf_set_lines(anchor_buf, 2, 3, false, {})
  equal(pins.position(anchor_buf, lost_pin), nil, "missing pin is unavailable")
  equal(pins.position(anchor_buf, lost_pin), nil, "missing pin stays unavailable without repeated mapping")
  vim.api.nvim_buf_set_lines(anchor_buf, 0, -1, false, original_lines)
  equal(pins.position(anchor_buf, lost_pin), { 2, 8 }, "restoring source recovers the original pin")
  vim.api.nvim_buf_delete(anchor_buf, { force = true })

  local call_buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(call_buf, 0, -1, false, {
    "let value = source", "    .map(|item| item + 1)", "    .unwrap_or(0);",
  })
  local chain = { start = { 0, 12 }, finish = { 2, 17 } }
  local call_focus = { containers = { chain }, places = { { range = chain, ranges = { chain }, slice = { chain }, direct_influence = {} } } }
  render.highlights()
  equal(render.show(call_buf, call_focus, { 2, 4 }, 200), nil, "dot in a method chain is not a focus target")
  equal(#vim.api.nvim_buf_get_extmarks(call_buf, render.namespace, 0, -1, {}), 0, "punctuation produces no expression-wide selection")
  check(render.show(call_buf, call_focus, { 2, 7 }, 200) ~= nil, "method name can select its result dependencies")
  local selected_call = vim.api.nvim_buf_get_extmarks(call_buf, render.namespace, 0, -1, { details = true })
  equal(#selected_call, 1, "whole call remains relevant without background highlighting it")
  equal({ selected_call[1][2], selected_call[1][3], selected_call[1][4].end_row, selected_call[1][4].end_col },
    { 2, 5, 2, 14 }, "selection background covers only unwrap_or")
  equal(render.show(call_buf, call_focus, { 1, 2 }, 200), nil, "whitespace does not select its enclosing expression")
  vim.api.nvim_buf_delete(call_buf, { force = true })

  local maybe_buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(maybe_buf, 0, -1, false, {
    "*a.borrow_mut() = input;", "let unrelated = 1;", "let seen = *b.borrow();",
  })
  local body = { start = { 0, 0 }, finish = { 2, 23 } }
  local written = { start = { 0, 0 }, finish = { 0, 24 } }
  local seen = { start = { 2, 4 }, finish = { 2, 8 } }
  local maybe_focus = { containers = { body }, places = { {
    range = seen, ranges = { seen }, slice = { { start = { 2, 0 }, finish = { 2, 23 } } },
    direct_influence = {}, maybe_slice = { written },
  } } }
  local function groups_at(row, col)
    local found = {}
    for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(maybe_buf, render.namespace, 0, -1, { details = true })) do
      local after_start = mark[2] < row or (mark[2] == row and mark[3] <= col)
      local before_end = mark[4].end_row > row or (mark[4].end_row == row and mark[4].end_col > col)
      if after_start and before_end then found[mark[4].hl_group] = true end
    end
    return found
  end
  local maybe_hl = vim.api.nvim_get_hl(0, { name = "FlowistryMaybe", link = true })
  local dim_hl = vim.api.nvim_get_hl(0, { name = "FlowistryDim", link = true })
  check(not maybe_hl.link and maybe_hl.fg ~= nil and maybe_hl.fg ~= dim_hl.fg, "possible writes have their own color")
  check(render.show(maybe_buf, maybe_focus, { 2, 5 }, 200) ~= nil, "maybe focus selects the reader")
  equal(groups_at(0, 3), { FlowistryMaybe = true }, "possible shared-handle write is tinted, not dimmed")
  equal(groups_at(1, 5), { FlowistryDim = true }, "unrelated code stays dimmed")
  render.show(maybe_buf, maybe_focus, { 2, 5 }, 200, false, false)
  equal(groups_at(0, 3), { FlowistryDim = true }, "show_maybe = false dims possible writes like unrelated code")
  render.show(maybe_buf, { containers = maybe_focus.containers, places = { {
    range = seen, ranges = { seen }, slice = maybe_focus.places[1].slice, direct_influence = {},
  } } }, { 2, 5 }, 200)
  equal(groups_at(0, 3), { FlowistryDim = true }, "responses without maybe_slice render as before")
  vim.api.nvim_buf_delete(maybe_buf, { force = true })

  setup()
  equal(flow.indicator(), "Flowistry: OFF", "indicator clearly shows disabled mode")
  local dim = vim.api.nvim_get_hl(0, { name = "FlowistryDim", link = true })
  check(not dim.link and dim.fg ~= nil, "dimmed code has its own color instead of linking to comments")
  move(2, 8)
  equal(vim.fn.getcompletion("Flow o", "cmdline"), { "on", "off" }, "short command completes on/off")
  vim.cmd("Flow on")
  await(function() return flow.status() == "active" end, "focus completes through actual subprocess/base64/gzip transport")
  local first = marks()
  check(#first > 0, "focus produces real extmarks")
  local dim_y = false
  for _, mark in ipairs(first) do
    check(mark[4].hl_group ~= "FlowistryInfluence", "direct-influence background blocks are disabled by default")
    if mark[4].hl_group == "FlowistryDim" and mark[2] <= 2 and mark[4].end_row >= 2 then dim_y = true end
    check(mark[4].priority >= 200, "decorations override syntax and semantic highlights")
  end
  check(dim_y, "unrelated y code is dimmed")
  equal(flow.indicator(), "Flowistry: ON", "indicator clearly shows active mode")
  equal(#calls(), 2, "one spans and one focus analysis")
  equal(calls()[1][2], file, "paths containing spaces remain a single argv entry")
  move(3, 8)
  await(function() return flow.status() == "active" and not vim.deep_equal(marks(), first) end, "cursor movement updates cached focus")
  equal(#calls(), 2, "cursor movement within function avoids backend calls")
  vim.cmd.write()
  await(function() return flow.status() == "active" end, "unchanged save restores memory analysis")
  equal(#calls(), 2, "unchanged save does not launch the backend")
  vim.api.nvim_buf_set_lines(buf, 1, 2, false, { "    let x = 42;" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = buf })
  await(function() return flow.status() == "waiting for save" end, "temporary edit suspends analysis")
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, source)
  vim.cmd.write()
  await(function() return flow.status() == "active" end, "restoring saved content restores memory analysis")
  equal(#calls(), 2, "edit and undo before save do not discard analysis")
  vim.cmd("Flow pin")
  equal(flow.status(), "pinned", "pin enables pinned mode")
  vim.cmd("Flow pin")
  equal(flow.status(), "active", "pin on the same variable toggles off")
  vim.cmd("Flow pin")
  move(2, 8)
  vim.cmd("Flow pin")
  equal(flow.status(), "pinned", "pin on a different variable moves the pin")
  local moved_pin = false
  for _, mark in ipairs(marks()) do
    if mark[4].hl_group == "FlowistryFocus" and mark[2] == 1 then moved_pin = true end
  end
  check(moved_pin, "repinning selects the new variable")
  move(3, 8)
  vim.cmd("Flow pin")
  equal(#calls(), 2, "pin toggles and repinning reuse cached analysis")
  local pinned = marks()
  move(2, 8)
  vim.wait(60, function() return false end)
  -- Extmark IDs may change on redraw; compare coordinates and options.
  local function without_ids(items) for _, m in ipairs(items) do m[1] = 0 end; return items end
  equal(without_ids(marks()), without_ids(pinned), "pin stays on the original variable")
  equal(flow.status(), "pinned", "pinned status")
  equal(flow.indicator(), "Flowistry: PINNED", "indicator clearly shows pinned mode")
  vim.cmd("Flow unpin")
  equal(flow.status(), "active", "unpin restores cursor tracking")
  move(7, 0)
  await(function() return flow.status() == "outside function" end, "cursor outside functions clears focus")
  equal(#marks(), 0, "no dimming outside a function")
  move(10, 12)
  await(function() return flow.status() == "active" end, "nested function analyzed")
  equal(calls()[3][3], "9", "nested function request uses zero-based row")
  move(2, 8)
  await(function() return flow.status() == "active" end, "returning to function reuses cache")
  equal(#calls(), 3, "both function results remain cached")

  vim.api.nvim_buf_set_text(buf, 1, 12, 1, 13, { "4" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = buf })
  await(function() return flow.status() == "waiting for save" end, "unsaved edits suspend analysis")
  check(#marks() > 0, "edits preserve the last successful highlights")
  check(flow.is_stale(), "preserved highlights are explicitly marked stale")
  check(vim.bo.modified, "plugin never saves user changes")
  vim.cmd.write()
  await(function() return flow.status() == "active" end, "save reanalyzes the buffer")
  check(not flow.is_stale(), "successful saved analysis clears stale marker")
  equal(#calls(), 5, "save invalidates spans and function cache")

  vim.cmd("Flow pin")
  vim.api.nvim_exec_autocmds("InsertEnter", { buffer = buf })
  check(#marks() > 0, "entering insert mode preserves highlights")
  vim.api.nvim_buf_set_text(buf, 1, 12, 1, 13, { "6" })
  vim.api.nvim_exec_autocmds("TextChangedI", { buffer = buf })
  await(function() return flow.status() == "waiting for save" end, "typing keeps analysis enabled but stale")
  move(3, 8)
  check(require("flowistry.statusline").text():find("saved analysis", 1, true), "stale state is visible in statusline")
  vim.cmd.write()
  await(function() return flow.status() == "pinned" end, "saving refreshes analysis without clearing pin")
  local saved_pin = false
  for _, mark in ipairs(marks()) do
    if mark[4].hl_group == "FlowistryFocus" and mark[2] == 1 then saved_pin = true end
  end
  check(saved_pin, "pin survives editing while cursor moved to another variable")
  -- rust.vim uses setline() even when rustfmt returned identical source. That
  -- moves ordinary extmarks to EOL; save must recover the actual source anchor.
  local formatter = vim.api.nvim_create_autocmd("BufWritePre", {
    buffer = buf, callback = function() vim.fn.setline(1, vim.api.nvim_buf_get_lines(buf, 0, -1, false)) end,
  })
  for _ = 1, 3 do
    vim.cmd.write()
    await(function() return flow.status() == "pinned" end, "formatter saves retain the pinned variable")
    local selected = false
    for _, mark in ipairs(marks()) do
      if mark[4].hl_group == "FlowistryFocus" and mark[2] == 1 and mark[3] == 8 then selected = true end
    end
    check(selected, "formatter rewrite does not move the pin to end of line")
  end
  vim.api.nvim_del_autocmd(formatter)
  local before_removal = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  vim.api.nvim_buf_set_text(buf, 1, 8, 1, 9, { "removed" })
  vim.cmd.write()
  await(function() return flow.status() == "pinned target unavailable" end, "removed target is reported instead of following the cursor")
  check(require("flowistry.statusline").text():find("pinned target unavailable", 1, true), "missing-pin status is visible")
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, before_removal)
  vim.cmd.write()
  await(function() return flow.status() == "pinned" end, "restoring deleted target recovers the pin")
  vim.cmd("Flow unpin")
  move(2, 8)
  await(function() return flow.status() == "active" end, "unpin resumes after editing")

  -- A dirty dependency must suspend focus even when the focused file is saved.
  local other_file = temp .. "/src/other.rs"
  vim.fn.writefile({ "pub const VALUE: u32 = 1;" }, other_file)
  local other = vim.fn.bufadd(other_file)
  vim.fn.bufload(other)
  vim.api.nvim_buf_set_lines(other, 0, -1, false, { "pub const VALUE: u32 = 2;" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = other })
  await(function() return flow.status() == "waiting for save" end, "dirty dependency suspends saved current buffer")
  check(#marks() > 0 and flow.is_stale(), "dirty dependency preserves saved highlights with stale status")
  vim.api.nvim_buf_call(other, function() vim.cmd("silent write") end)
  await(function() return flow.status() == "active" end, "saving dependency resumes analysis")
  vim.api.nvim_buf_delete(other, { force = true })

  -- Workspace manifests are also disk inputs to the compiler.
  local manifest = vim.fn.bufadd(temp .. "/Cargo.toml")
  vim.fn.bufload(manifest)
  vim.api.nvim_buf_set_lines(manifest, -1, -1, false, { "# edited manifest" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = manifest })
  await(function() return flow.status() == "waiting for save" end, "dirty manifest suspends analysis")
  vim.api.nvim_buf_call(manifest, function() vim.cmd("silent write") end)
  await(function() return flow.status() == "active" end, "saving manifest resumes analysis")
  vim.api.nvim_buf_delete(manifest, { force = true })

  local before_switch = #calls()
  vim.cmd("Flow pin")
  local another_file = temp .. "/src/another.rs"
  vim.fn.writefile(source, another_file)
  vim.cmd.edit(vim.fn.fnameescape(another_file))
  local another_buf = vim.api.nvim_get_current_buf()
  equal(flow.status(buf), "pinned", "opening a Rust file preserves another buffer's pin")
  vim.bo.filetype = "rust"
  move(2, 8)
  vim.cmd("Flow on")
  await(function() return flow.status() == "active" end, "second Rust buffer has independent analysis")
  local after_second = #calls()
  vim.api.nvim_set_current_buf(buf)
  await(function() return flow.status() == "pinned" end, "returning to Rust buffer restores its pin")
  equal(#calls(), after_second, "returning to pinned buffer does not recompile")
  vim.api.nvim_set_current_buf(another_buf)
  await(function() return flow.status() == "active" end, "second buffer retains enabled state")
  equal(#calls(), after_second, "switching between analyzed Rust buffers uses both caches")
  vim.cmd("Flow off")
  vim.api.nvim_set_current_buf(buf)
  vim.cmd("Flow unpin")
  vim.api.nvim_set_current_buf(another_buf)
  equal(flow.status(), "off", "explicit off state survives buffer switches")
  vim.api.nvim_set_current_buf(buf)
  vim.api.nvim_buf_delete(another_buf, { force = true })
  before_switch = #calls()
  local scratch = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_set_current_buf(scratch)
  equal(flow.status(), "off", "focus state is per buffer")
  vim.api.nvim_set_current_buf(buf)
  await(function() return flow.status() == "active" end, "buffer switch preserves cached focus")
  equal(#calls(), before_switch, "buffer switch does not recompile")
  vim.api.nvim_buf_delete(scratch, { force = true })
  flow.disable()
  equal(flow.status(), "off", "disable clears state")
  equal(#marks(), 0, "disable clears extmarks")

  flow.enable()
  await(function() return flow.status() == "active" end, "analysis restored for invalidation checks")
  vim.cmd("Flow pin")
  local unrelated = vim.api.nvim_create_buf(true, false)
  vim.api.nvim_buf_set_name(unrelated, temp .. "-unrelated/src/main.rs")
  vim.bo[unrelated].filetype = "rust"
  vim.api.nvim_buf_set_lines(unrelated, 0, -1, false, { "fn unrelated() {}" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = unrelated })
  equal(flow.status(buf), "pinned", "editing another project does not invalidate this project's pin")
  vim.api.nvim_buf_delete(unrelated, { force = true })
  local before_reload = #calls()
  vim.api.nvim_exec_autocmds("BufReadPost", { buffer = buf })
  await(function() return flow.status() == "active" end, "reload of known source invalidates and reanalyzes")
  check(#calls() > before_reload, "reload does not reuse stale analysis")
  flow.disable()

  flow.setup({ command = command, auto_enable = false, debounce_ms = 5, progress = true,
    env = { FLOWISTRY_TEST_DELAY = "500", FLOWISTRY_TEST_LOG = log } })
  local before_cancel = #calls()
  local editing_win = vim.api.nvim_get_current_win()
  flow.enable()
  await(function() return #calls() > before_cancel end, "backend actually started before cancellation")
  check(flow.indicator():match("Finding functions%.%.%. %d+%.%ds") ~= nil, "loading indicator identifies stage and elapsed time")
  local popup
  await(function()
    for _, win in ipairs(vim.api.nvim_list_wins()) do
      if vim.api.nvim_win_get_config(win).relative == "editor" then popup = win; return true end
    end
  end, "analysis opens a progress popup")
  equal(vim.api.nvim_get_current_win(), editing_win, "analysis popup never steals editing focus")
  check(table.concat(vim.api.nvim_buf_get_lines(vim.api.nvim_win_get_buf(popup), 0, -1, false)):match("Finding functions%.%.%. %d+%.%ds"),
    "popup shows analysis phase and elapsed time")
  flow.disable()
  check(not vim.api.nvim_win_is_valid(popup), "disabling analysis closes its popup")
  vim.wait(400, function() return false end)
  equal(#marks(), 0, "late results cannot redraw disabled mode")
  equal(flow.status(), "off", "late results cannot resurrect state")

  for _, mode in ipairs({ "exit", "diagnostics", "base64", "gzip", "error", "schema", "json" }) do
    setup({ FLOWISTRY_TEST_MODE = mode })
    flow.enable()
    await(function() return flow.status() == "error" end, "failure surfaced: " .. mode)
    equal(#marks(), 0, "failure clears highlights: " .. mode)
    if mode == "diagnostics" then
      check(notes[#notes]:find("Flowistry produced no analysis", 1, true), "empty compiler output reports diagnostics instead of a base64 error")
    end
  end
  flow.log()
  check(vim.bo.buftype == "nofile" and not vim.bo.modifiable, "error log opens a readonly scratch buffer")
  vim.cmd.close()

  local listed
  for _, tabulated in ipairs({ false, true }) do
    setup(tabulated and { FLOWISTRY_TEST_RANGE_TABLE = "1" } or nil)
    move(2, 8)
    flow.enable()
    await(function() return flow.status() == "active" end, "focus completes, range table: " .. tostring(tabulated))
    check(#marks() > 0, "focus highlights, range table: " .. tostring(tabulated))
    if tabulated then equal(marks(), listed, "a range table renders the same highlights as range lists") end
    listed = marks()
    flow.disable()
  end

  setup({ FLOWISTRY_TEST_DELAY = "250" })
  local before_edit = #calls()
  flow.enable()
  await(function() return #calls() > before_edit end, "backend actually started before unsignaled edit")
  vim.api.nvim_buf_set_text(buf, 1, 12, 1, 13, { "5" })
  -- Do not dispatch TextChanged: callback must independently check changedtick.
  await(function() return flow.status() == "waiting for save" end, "changedtick rejects an in-flight stale result")
  equal(#marks(), 0, "stale result never renders")
  vim.cmd.write()
  await(function() return flow.status() == "active" end, "recovers after stale request and save")

  local completed = false
  backend.request({ root = temp, env = {}, command = { "/definitely/missing/flowistry" } }, { "spans", file },
    { timeout_ms = 1000, gzip = "gzip" }, function(err)
      check(type(err) == "string", "missing executable returns an actionable error")
      completed = true
    end)
  await(function() return completed end, "spawn failure callback delivered")
  flow.disable()

  for _, max_lines in ipairs({ 600, 1 }) do
    flow.setup({ command = command, auto_enable = false, batch = true, batch_max_lines = max_lines, debounce_ms = 5,
      env = { FLOWISTRY_TEST_LOG = log } })
    move(2, 8)
    local before_batch = #calls()
    flow.enable()
    await(function() return flow.status() == "active" end, "combined backend response renders")
    equal(#calls(), before_batch + 1, "initial focus needs just one compiler request")
    equal(calls()[#calls()][1], "file-focus", "combined command used")
    if max_lines == 1 then
      equal(calls()[#calls()][3], "1", "large file asks for selected function only")
    end
    move(10, 12)
    await(function()
      local all = marks()
      for _, m in ipairs(all) do if m[2] == 9 and m[4].hl_group == "FlowistryFocus" then return true end end
      return false
    end, "nested function selection works with batch result")
    equal(#calls(), before_batch + (max_lines == 1 and 2 or 1), "small-file functions cached, large-file functions loaded on demand")
    if max_lines == 1 then
      equal(calls()[#calls()][1], "file-focus", "later large-file functions also use scoped fact collection")
    end
    move(2, 8)
    await(function() return flow.status() == "active" end, "returning to first batch function restores focus")
    equal(#calls(), before_batch + (max_lines == 1 and 2 or 1), "loading another function retains earlier cached analysis")
    move(8, 3)
    await(function() return flow.status() == "analysis unavailable" end, "individual body error does not discard other cached functions")
    flow.disable()
  end

  -- At the exclusive end of a nested body, request its enclosing body even
  -- though the compiler's zero-width cursor containment would select the child.
  flow.setup({ command = command, auto_enable = false, batch = true, batch_max_lines = 1, debounce_ms = 5,
    env = { FLOWISTRY_TEST_LOG = log, FLOWISTRY_TEST_MODE = "inclusive_body_end" } })
  move(2, 8)
  flow.enable()
  await(function() return flow.status() == "active" end, "boundary fixture initially analyzed")
  move(11, 1)
  await(function() return flow.status() == "analysis unavailable" end,
    "closure-end boundary loads the parent result without a protocol failure")
  move(10, 12)
  await(function() return flow.status() == "active" end, "nested body still loads after boundary navigation")
  flow.disable()

  -- Session setup preserves the existing statusline and leaves the winbar alone.
  vim.env.FLOWISTRY_BACKEND_EXE = "/not/invoked/in/this/test"
  vim.wo.statusline = "existing buffers"
  local previous_winbar = vim.wo.winbar
  local previous_setup, previous_enable = flow.setup, flow.enable
  local session_opts, forced_enable
  vim.g.flowistry_config = { auto_enable = false, debounce_ms = 77 }
  flow.setup = function(opts) session_opts = opts end
  flow.enable = function() forced_enable = true end
  dofile(repo .. "/scripts/session.lua")
  flow.setup, flow.enable = previous_setup, previous_enable
  vim.g.flowistry_config = nil
  equal(session_opts.auto_enable, false, "launcher honors the auto-enable override")
  equal(session_opts.debounce_ms, 77, "launcher merges user configuration")
  check(not forced_enable, "launcher does not force activation after setup")
  local statusline = require("flowistry.statusline")
  statusline.attach()
  local rendered_bar = vim.api.nvim_eval_statusline(vim.wo.statusline, {}).str
  equal(rendered_bar, "existing buffers", "disabled Flowistry leaves existing status contents alone")
  _G.FlowistryTestStatusline = function() return "computed buffers" end
  vim.wo.statusline = "%!v:lua.FlowistryTestStatusline()"
  statusline.attach()
  statusline.attach()
  equal(vim.api.nvim_eval_statusline(vim.wo.statusline, {}).str, "computed buffers", "computed statusline remains valid")
  setup()
  move(2, 8)
  flow.enable()
  await(function() return flow.status() == "active" end, "computed statusline test has active analysis")
  equal(vim.api.nvim_eval_statusline(vim.wo.statusline, {}).str, "computed buffers flowistry", "computed statusline includes active label once")
  flow.disable()
  _G.FlowistryTestStatusline = nil
  vim.wo.statusline = ""
  equal(vim.wo.winbar, previous_winbar, "session does not add a top bar")
  vim.bo.filetype = "help"
  vim.bo.buftype = "help"
  equal(statusline.text(), "", "status is hidden in non-source buffers")
  vim.bo.filetype, vim.bo.buftype = "rust", ""
  vim.env.FLOWISTRY_BACKEND_EXE = nil

  flow.setup({ command = command, auto_enable = false, timeout_ms = 50, env = { FLOWISTRY_TEST_DELAY = "500" } })
  flow.enable()
  await(function() return flow.status() == "error" end, "timeout becomes a recoverable error")
  flow.disable()

  -- Verify the default compiler/workspace preparation contract without requiring
  -- a globally installed Rust compiler. Transport tests above use real processes.
  local native_system = vim.system
  local prep_calls = {}
  vim.system = function(argv, opts, cb)
    prep_calls[#prep_calls + 1] = { argv = vim.deepcopy(argv), opts = vim.deepcopy(opts) }
    local stdout = argv[1] == "rustc" and "/compiler/target/lib\n/compiler\n" or (temp .. "/Cargo.toml\n")
    vim.schedule(function() cb({ code = 0, stdout = stdout, stderr = "" }) end)
    return { kill = function() end }
  end
  local prepared, prepare_error
  backend.context(temp .. "/member", { toolchain = "nightly-2026-05-01", env = {}, timeout_ms = 1000 }, function(err, context)
    prepare_error, prepared = err, context
  end)
  local ready = vim.wait(1000, function() return prepared ~= nil or prepare_error ~= nil end, 10)
  vim.system = native_system
  check(ready and prepared ~= nil, "compiler context prepared")
  equal(prepared.root, temp, "Cargo workspace root supersedes nearest member")
  equal(prepared.command, { "cargo", "+nightly-2026-05-01", "flowistry" }, "pinned compiler used for backend")
  equal(prepared.env.SYSROOT, "/compiler", "compiler sysroot is passed to backend")
  equal(prepared.env.RUSTUP_TOOLCHAIN, "nightly-2026-05-01", "nested Cargo processes inherit matching compiler")
  equal(prep_calls[1].argv, { "rustc", "+nightly-2026-05-01", "--print", "target-libdir", "--print", "sysroot" },
    "compiler library discovery uses argv without shell parsing")
  equal(prep_calls[2].argv, { "cargo", "+nightly-2026-05-01", "locate-project", "--workspace", "--message-format", "plain" },
    "workspace discovery uses Cargo")

  -- Automatic enablement is the default, but an explicit off remains sticky.
  move(2, 8)
  flow.setup({ command = command, debounce_ms = 5, env = { FLOWISTRY_TEST_LOG = log } })
  await(function() return flow.status() == "active" end, "setup enables the current Rust buffer by default")
  local native_select, menu, choose = vim.ui.select
  vim.ui.select = function(items, opts, callback) menu, choose = items, callback end
  vim.cmd("Flow")
  equal(menu, { "on", "off", "pin", "unpin", "toggle", "refresh", "types", "project", "start", "stop", "log" }, "bare Flow offers its actions")
  equal(flow.status(), "active", "opening the action menu does not toggle flow")
  choose(nil)
  equal(flow.status(), "active", "cancelling the action menu preserves state")
  vim.cmd("Flow")
  choose("pin")
  equal(flow.status(), "pinned", "action menu dispatches the selected command")
  vim.cmd("Flowistry")
  equal(flow.status(), "pinned", "bare long command also opens a menu without toggling")
  choose("off")
  vim.ui.select = native_select
  local off_calls = #calls()
  for _, event in ipairs({ "BufEnter", "FileType", "BufWritePost" }) do
    vim.api.nvim_exec_autocmds(event, { buffer = buf })
  end
  vim.wait(100, function() return false end)
  equal(flow.status(), "off", "explicit off survives enter, filetype and save events")
  equal(#calls(), off_calls, "explicit off does not start background analysis")
  local auto_file = temp .. "/src/auto.rs"
  vim.fn.writefile(source, auto_file)
  vim.cmd.edit(vim.fn.fnameescape(auto_file))
  vim.bo.filetype = "rust"
  local auto_buf = vim.api.nvim_get_current_buf()
  vim.api.nvim_win_set_cursor(0, { 2, 8 })
  await(function() return flow.status() == "active" end, "new Rust buffers enable automatically")
  vim.api.nvim_set_current_buf(buf)
  vim.wait(100, function() return false end)
  equal(flow.status(), "off", "returning to an explicitly disabled buffer keeps it off")
  vim.cmd("Flow on")
  await(function() return flow.status() == "active" end, "explicit on restores a disabled buffer")
  vim.api.nvim_buf_delete(auto_buf, { force = true })
  setup()
  vim.api.nvim_exec_autocmds("BufEnter", { buffer = buf })
  vim.wait(100, function() return false end)
  equal(flow.status(), "off", "auto_enable=false leaves activation to the user")

  setup({ FLOWISTRY_TEST_DELAY = "250" })
  flow.enable()
  vim.api.nvim_buf_delete(buf, { force = true })
  vim.wait(350, function() return false end)
  equal(flow.status(buf), "off", "buffer disposal cancels pending work")
end

local ok, err = xpcall(run, debug.traceback)
flow.disable()
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d assertions"):format(passed))
vim.cmd("qa!")
