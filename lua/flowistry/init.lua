local M = {}
local backend = require("flowistry.backend")
local ranges = require("flowistry.ranges")
local render = require("flowistry.render")
local progress = require("flowistry.progress")
local pins = require("flowistry.pin")
local states = {}
local suspended = {}
local disabled = {}
local configured = false
local uv = vim.uv or vim.loop
local progress_timer
local defaults = {
  auto_enable = true,
  toolchain = "nightly-2026-05-01",
  context_mode = nil, -- Backend default (SigOnly); Recurse opts into callee summaries.
  cache = true, -- Persistent compiler-validated results in the shared backend.
  cache_dir = nil, -- nil uses the backend's XDG cache location.
  command = nil, -- Full backend argv; bypasses rustup/sysroot discovery when set.
  root = nil, -- Optional workspace root for externally managed backend commands.
  batch = false, -- Packaged backend supports file-focus; vanilla upstream does not.
  batch_max_lines = 600, -- Large files retain on-demand function analysis.
  env = {},
  gzip = "gzip",
  debounce_ms = 120,
  timeout_ms = 180000,
  priority = 200,
  show_influence = false,
  show_maybe = true,
  progress = false, -- Session launcher enables analysis progress popups.
}
local config = vim.deepcopy(defaults)
local update

local function current() return vim.api.nvim_get_current_buf() end
local function notify(message, level) vim.notify("Flowistry: " .. message, level or vim.log.levels.INFO) end
local function is_rust(buf)
  return vim.bo[buf].filetype == "rust" or vim.api.nvim_buf_get_name(buf):match("%.rs$") ~= nil
end

local function stop(state)
  progress.close(state)
  state.epoch = state.epoch + 1
  if state.operation then state.operation:cancel(); state.operation = nil end
  if state.timer then
    state.timer:stop()
    if not state.timer:is_closing() then state.timer:close() end
    state.timer = nil
  end
  state.busy = false
end

local function clear_pin(state)
  state.mark = nil
  pins.clear(state.buf)
end

local function pin_position(state)
  if not state.mark then return nil end
  return pins.position(state.buf, state.mark)
end

local function dispose(buf)
  if states[buf] then stop(states[buf]); clear_pin(states[buf]); states[buf] = nil end
  if suspended[buf] then stop(suspended[buf]); clear_pin(suspended[buf]); suspended[buf] = nil end
  render.clear(buf)
end

local function all_states()
  return vim.tbl_extend("force", suspended, states)
end

local function invalidate(state, preserve)
  stop(state)
  if preserve and state.bodies and not state.retained then
    state.retained = { bodies = state.bodies, inputs = {} }
  elseif not preserve then
    state.retained = nil
  end
  state.stale = preserve and (state.stale or state.slice ~= nil) or false
  state.bodies, state.slice, state.error = nil, nil, nil
  state.cached = nil
  state.status = "idle"
  if not preserve then clear_pin(state); render.clear(state.buf) end
end

local function disk_source(name)
  local ok, lines = pcall(vim.fn.readfile, name, "b")
  return ok and lines or false
end

local function restore_unchanged(state)
  local retained = state.retained
  if not retained or not next(retained.inputs) then return end
  for name, source in pairs(retained.inputs) do
    if source == false or not vim.deep_equal(source, disk_source(name)) then return end
  end
  stop(state)
  state.bodies, state.retained, state.error = retained.bodies, nil, nil
  state.stale = false
end

local function inside_root(name, root)
  if name == "" or not root then return false end
  root = uv.fs_realpath(root) or root
  name = uv.fs_realpath(name) or vim.fs.normalize(name)
  return name:sub(1, #root + 1) == root .. "/"
end

local function auto_enable()
  local buf = current()
  if not config.auto_enable or disabled[buf] or states[buf] or not is_rust(buf) or vim.bo[buf].buftype ~= "" then return end
  local name = vim.api.nvim_buf_get_name(buf)
  if name == "" or vim.fn.filereadable(name) ~= 1 then return end
  if config.root then
    if not inside_root(name, config.root) then return end
  elseif not vim.fs.root(name, "Cargo.toml") then return end
  M.enable(true)
end

local function dirty(root)
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].modified then
      local name = vim.fs.normalize(vim.api.nvim_buf_get_name(buf))
      name = uv.fs_realpath(name) or name
      local relevant = is_rust(buf) or name:match("Cargo%.toml$") or name:match("Cargo%.lock$")
      if relevant and inside_root(name, root) then return true end
    end
  end
  return false
end

local function fail(state, err)
  progress.close(state)
  state.busy, state.operation, state.slice = false, nil, nil
  state.status, state.error = "error", tostring(err)
  if not state.stale then render.clear(state.buf) end
  notify("Analysis failed; use :Flowistry log for details.\n" .. state.error:match("[^\n]+"), vim.log.levels.ERROR)
end

-- A generation guards every subprocess callback, including decode and startup.
local function callback(state, handler, phase)
  local epoch = state.epoch
  local tick = vim.api.nvim_buf_get_changedtick(state.buf)
  local name = vim.api.nvim_buf_get_name(state.buf)
  state.busy, state.status = true, "loading"
  state.phase, state.started = phase or "Preparing compiler", uv.hrtime()
  vim.cmd("redrawstatus")
  return function(err, value)
    if states[state.buf] ~= state or state.epoch ~= epoch or not vim.api.nvim_buf_is_valid(state.buf) then return end
    if vim.api.nvim_buf_get_changedtick(state.buf) ~= tick or vim.api.nvim_buf_get_name(state.buf) ~= name then
      invalidate(state, true)
      state.status = "waiting for save"
      return
    end
    state.busy, state.operation = false, nil
    progress.close(state)
    state.last_ms = (uv.hrtime() - state.started) / 1e6
    if err then fail(state, err); return end
    local ok, reason = pcall(handler, value)
    if not ok then fail(state, "Invalid analysis response: " .. tostring(reason)); return end
    if type(value) == "table" and value.cache then state.cache_stats = value.cache end
    state.status = "idle"
    if current() == state.buf then update(state) end
    vim.cmd("redrawstatus")
  end
end

local function prepare_focus(state, value)
  assert(type(value.place_info) == "table" and vim.islist(value.place_info), "Missing place_info")
  assert(type(value.containers) == "table" and vim.islist(value.containers), "Missing containers")
  local source_id = value.containers[1] and value.containers[1].filename
  local convert = ranges.converter(state.buf, state.context.root, source_id)
  local result = { containers = ranges.convert_list(value.containers, convert), places = {} }
  -- Newer backends send each distinct range once, in `ranges`, and places refer to them
  -- by 0-based index. Convert each once; converted ranges are shared, never mutated.
  local place_range, place_list = convert, function(items) return ranges.convert_list(items, convert) end
  if value.ranges ~= nil then
    assert(type(value.ranges) == "table" and vim.islist(value.ranges), "Invalid range table")
    local converted = {}
    for i, range in ipairs(value.ranges) do converted[i - 1] = convert(range) or false end
    place_range = function(index)
      assert(type(index) == "number" and converted[index] ~= nil, "Invalid range index")
      return converted[index] or nil
    end
    place_list = function(indices)
      assert(type(indices) == "table" and vim.islist(indices), "Expected a list of range indices")
      local list = {}
      for _, index in ipairs(indices) do list[#list + 1] = place_range(index) end
      return list
    end
  end
  for _, place in ipairs(value.place_info) do
    local range = place_range(place.range)
    if range then
      result.places[#result.places + 1] = {
        range = range,
        ranges = place_list(place.ranges),
        slice = place_list(place.slice),
        direct_influence = place_list(place.direct_influence),
        maybe_slice = place_list(place.maybe_slice or {}),
      }
    end
  end
  return result
end

update = function(state)
  if states[state.buf] ~= state or current() ~= state.buf then return end
  if vim.api.nvim_get_mode().mode:sub(1, 1) == "i" then
    state.status = state.stale and "waiting for save" or "editing"
    return
  end
  local root = state.context and state.context.root or state.root
  if vim.bo[state.buf].modified or dirty(root) then
    state.stale = state.stale or state.slice ~= nil
    state.status = "waiting for save"
    return
  end
  if not state.busy then restore_unchanged(state) end
  if state.busy or state.error then return end
  local pinned = pin_position(state)
  if state.mark and not pinned then
    render.clear(state.buf)
    state.slice, state.stale, state.status = nil, false, "pinned target unavailable"
    return
  end
  if not state.context then
    state.operation = backend.context(state.root, config, callback(state, function(context)
      state.context = context
    end))
    return
  end
  local filename = vim.api.nvim_buf_get_name(state.buf)
  if not state.bodies then
    if config.batch then
      local args = { "file-focus", filename }
      local phase = "Analyzing file"
      if vim.api.nvim_buf_line_count(state.buf) > config.batch_max_lines then
        local cursor = vim.api.nvim_win_get_cursor(0)
        if pinned then cursor = { pinned[1] + 1, pinned[2] } end
        local pos = ranges.position(state.buf, cursor)
        vim.list_extend(args, { tostring(pos[1]), tostring(pos[2]) })
        phase = "Analyzing function"
      end
      local request_config = vim.tbl_extend("force", config, { cache_refresh = state.cache_refresh })
      state.operation = backend.request(state.context, args, request_config, callback(state, function(value)
        assert(type(value.bodies) == "table" and vim.islist(value.bodies), "Missing bodies")
        local source_id = value.bodies[1] and value.bodies[1].range.filename
        local convert = ranges.converter(state.buf, state.context.root, source_id)
        state.bodies = {}
        for _, item in ipairs(value.bodies) do
          local range = convert(item.range)
          if range then
            local focus = item.focus ~= vim.NIL and item.focus or nil
            assert(not focus or type(focus) == "table", "Invalid body result")
            state.bodies[#state.bodies + 1] = {
              range = range,
              focus = focus and focus.Ok and prepare_focus(state, focus.Ok) or nil,
              error = focus and focus.Err,
              cached = item.cached == true,
            }
          end
        end
        state.retained, state.cache_refresh = nil, nil
      end, phase))
      return
    end
    state.operation = backend.request(state.context, { "spans", filename }, config, callback(state, function(value)
      assert(type(value.spans) == "table" and vim.islist(value.spans), "Missing spans")
      local source_id = value.spans[1] and value.spans[1].filename
      local convert = ranges.converter(state.buf, state.context.root, source_id)
      state.bodies = {}
      for _, range in ipairs(ranges.convert_list(value.spans, convert)) do
        state.bodies[#state.bodies + 1] = { range = range }
      end
    end, "Finding functions"))
    return
  end
  local cursor = vim.api.nvim_win_get_cursor(0)
  local pos = pinned or { cursor[1] - 1, cursor[2] }
  local body = ranges.smallest(state.bodies, pos, function(item) return item.range end)
  if not body then
    render.clear(state.buf)
    state.stale = false
    state.slice, state.status = nil, "outside function"
    return
  end
  if not body.focus then
    if not state.stale then render.clear(state.buf) end
    if body.error then
      state.slice, state.status = nil, "analysis unavailable"
      return
    end
    local char = ranges.position(state.buf, { pos[1] + 1, pos[2] })
    state.operation = backend.request(state.context, {
      config.batch and "file-focus" or "focus", filename, tostring(char[1]), tostring(char[2]),
    }, vim.tbl_extend("force", config, { cache_refresh = state.cache_refresh }), callback(state, function(value)
      state.retained, state.cache_refresh = nil, nil
      if not config.batch then body.focus = prepare_focus(state, value); return end
      assert(type(value.bodies) == "table" and vim.islist(value.bodies), "Missing bodies")
      local convert = ranges.converter(state.buf, state.context.root, value.bodies[1] and value.bodies[1].range.filename)
      for _, item in ipairs(value.bodies) do
        if vim.deep_equal(convert(item.range), body.range) and item.focus ~= vim.NIL then
          if item.focus.Ok then
            body.focus = prepare_focus(state, item.focus.Ok)
            body.cached = item.cached == true
          elseif item.focus.Err then body.error = item.focus.Err
          else error("Invalid selected function result") end
          return
        end
      end
      error("Missing selected function result")
    end, "Analyzing function"))
    return
  end
  state.slice = render.show(
    state.buf, body.focus, pos, config.priority, config.show_influence, config.show_maybe
  )
  state.cached = body.cached == true
  state.stale = false
  state.status = state.slice and (state.mark and "pinned" or "active") or "no place"
end

local function schedule(state)
  if state.timer then
    state.timer:stop()
    if not state.timer:is_closing() then state.timer:close() end
  end
  state.timer = vim.defer_fn(function()
    state.timer = nil
    if states[state.buf] == state then update(state) end
  end, config.debounce_ms)
end

function M.setup(opts)
  local mode = opts and opts.context_mode
  assert(mode == nil or mode == "SigOnly" or mode == "Recurse", "context_mode must be SigOnly or Recurse")
  if progress_timer then progress_timer:stop(); progress_timer:close() end
  for _, state in pairs(all_states()) do stop(state); clear_pin(state); render.clear(state.buf) end
  states, suspended, disabled = {}, {}, {}
  config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), opts or {})
  assert(type(config.debounce_ms) == "number" and config.debounce_ms >= 0, "debounce_ms must be nonnegative")
  assert(type(config.timeout_ms) == "number" and config.timeout_ms > 0, "timeout_ms must be positive")
  assert(type(config.auto_enable) == "boolean", "auto_enable must be a boolean")
  assert(type(config.cache) == "boolean", "cache must be a boolean")
  assert(config.cache_dir == nil or (type(config.cache_dir) == "string" and config.cache_dir ~= ""), "cache_dir must be a nonempty path")
  assert(not config.command or (vim.islist(config.command) and #config.command > 0), "command must be an argv list")
  render.highlights()
  local group = vim.api.nvim_create_augroup("Flowistry", { clear = true })
  local observed_ticks, observed_names, saved_sources = {}, {}, {}
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) then
      observed_ticks[buf] = vim.api.nvim_buf_get_changedtick(buf)
      observed_names[buf] = vim.api.nvim_buf_get_name(buf)
      local name = observed_names[buf]
      if is_rust(buf) or name:match("Cargo%.toml$") or name:match("Cargo%.lock$") then
        saved_sources[name] = disk_source(name)
      end
    end
  end
  vim.api.nvim_create_autocmd({ "CursorMoved", "BufEnter" }, {
    group = group,
    callback = function(args) if states[args.buf] then schedule(states[args.buf]) end end,
  })
  vim.api.nvim_create_autocmd({ "BufEnter", "FileType", "BufWritePost" }, {
    group = group,
    callback = function() vim.schedule(auto_enable) end,
  })
  vim.api.nvim_create_autocmd({ "TextChanged", "TextChangedI", "TextChangedP", "BufWritePost", "BufReadPost", "BufFilePost" }, {
    group = group,
    callback = function(args)
      local previous_tick = observed_ticks[args.buf]
      local tick = vim.api.nvim_buf_get_changedtick(args.buf)
      observed_ticks[args.buf] = tick
      local name = vim.api.nvim_buf_get_name(args.buf)
      local previous_name = observed_names[args.buf]
      observed_names[args.buf] = name
      -- Reading a file for the first time does not change the compiler's disk
      -- inputs. Reloads of known buffers still invalidate stale analysis.
      if args.event == "BufReadPost" and previous_tick == nil then
        if is_rust(args.buf) or name:match("Cargo%.toml$") or name:match("Cargo%.lock$") then
          saved_sources[name] = disk_source(name)
        end
        return
      end
      -- TextChanged may be delivered when entering a buffer even though no edit
      -- happened since its read/write or the last observed change.
      if args.event:match("^TextChanged") and (previous_tick == tick
        or (previous_tick == nil and not vim.bo[args.buf].modified)) then return end
      -- Another Rust file or a manifest can change this function's analysis.
      if not is_rust(args.buf) and not name:match("Cargo%.toml$") and not name:match("Cargo%.lock$") then return end
      for _, state in pairs(all_states()) do
        local root = state.context and state.context.root or state.root
        if state.buf == args.buf or inside_root(name, root)
          or (args.event == "BufFilePost" and previous_name and inside_root(previous_name, root)) then
          invalidate(state, args.event ~= "BufReadPost" and args.event ~= "BufFilePost")
          if state.retained and state.retained.inputs[name] == nil then
            state.retained.inputs[name] = saved_sources[name] or false
          end
        end
        if args.event == "BufFilePost" and state.buf == args.buf then
          state.context = nil
          state.root = vim.fs.root(name, "Cargo.toml")
          if not state.root then
            M.disable(state.buf)
          end
        end
      end
      if args.event == "BufWritePost" or args.event == "BufReadPost" then saved_sources[name] = disk_source(name) end
      -- :wall and nvim_buf_call temporarily switch the current buffer while
      -- writing a dependency. Resolve the active editor after that switch ends.
      vim.schedule(function()
        if states[current()] then schedule(states[current()]) end
      end)
    end,
  })
  vim.api.nvim_create_autocmd("InsertEnter", {
    group = group,
    callback = function(args)
      if states[args.buf] then states[args.buf].status = "editing"; vim.cmd("redrawstatus") end
    end,
  })
  vim.api.nvim_create_autocmd("InsertLeave", {
    group = group,
    callback = function(args) if states[args.buf] then schedule(states[args.buf]) end end,
  })
  vim.api.nvim_create_autocmd({ "BufUnload", "BufWipeout" }, {
    group = group, callback = function(args)
      dispose(args.buf)
      if args.event == "BufWipeout" then
        observed_ticks[args.buf], observed_names[args.buf], disabled[args.buf] = nil, nil, nil
      end
    end,
  })
  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = group, callback = function()
      for _, state in pairs(states) do stop(state) end
      if progress_timer and not progress_timer:is_closing() then progress_timer:stop(); progress_timer:close() end
      progress_timer = nil
    end,
  })
  vim.api.nvim_create_autocmd("ColorScheme", { group = group, callback = render.highlights })
  configured = true
  progress_timer = uv.new_timer()
  progress_timer:start(200, 200, vim.schedule_wrap(function()
    local busy = false
    for _, state in pairs(states) do
      if state.busy then
        busy = true
        if config.progress then progress.update(state) end
      end
    end
    if busy then vim.cmd("redrawstatus") end
  end))
  vim.schedule(auto_enable)
end

function M.enable(quiet)
  if not configured then M.setup() end
  local buf = current()
  if states[buf] then update(states[buf]); return end
  local filename = vim.api.nvim_buf_get_name(buf)
  if not is_rust(buf) or filename == "" or vim.bo[buf].buftype ~= "" then
    notify("Open a saved Rust file in a Cargo project first.", vim.log.levels.WARN); return
  end
  local root = config.root or vim.fs.root(filename, "Cargo.toml")
  if root then root = vim.fs.normalize(root) end
  if not root then notify("No Cargo.toml found above this file.", vim.log.levels.WARN); return end
  if vim.fn.filereadable(filename) ~= 1 then notify("Save this file before enabling focus mode.", vim.log.levels.WARN); return end
  local state = suspended[buf] or { buf = buf, root = root, epoch = 0, status = "idle" }
  if state.error then invalidate(state, true) end
  suspended[buf] = nil
  disabled[buf] = nil
  states[buf] = state
  update(state)
  if state.status == "waiting for save" and not quiet then notify("Save modified Rust buffers in this project to analyze them.") end
end

function M.disable(buf)
  buf = buf or current()
  disabled[buf] = true
  local state = states[buf]
  if state then
    stop(state)
    clear_pin(state)
    states[buf], suspended[buf] = nil, state
  end
  render.clear(buf)
end

function M.toggle()
  if states[current()] then M.disable() else M.enable() end
end

function M.mark()
  if not states[current()] then M.enable() end
  local state = states[current()]
  if not state then return end
  local cursor = vim.api.nvim_win_get_cursor(0)
  local pos = { cursor[1] - 1, cursor[2] }
  if not ranges.token(state.buf, pos) then return end
  if state.mark and pins.contains(state.buf, state.mark, pos) then
    clear_pin(state)
  else
    state.mark = pins.set(state.buf, pos, state.mark)
  end
  update(state)
end

function M.unmark()
  local state = states[current()]
  if state then clear_pin(state); update(state) end
end

function M.refresh()
  -- Also refresh sibling buffers whose dependencies may have changed on disk.
  for _, state in pairs(all_states()) do
    invalidate(state)
    state.context, state.cache_stats, state.cache_refresh = nil, nil, true
  end
  if states[current()] then update(states[current()]) else M.enable() end
end

function M.cache_status(buf)
  local state = states[buf or current()]
  return state and state.cache_stats and vim.deepcopy(state.cache_stats) or nil
end

function M.status(buf)
  local state = states[buf or current()]
  return state and state.status or "off"
end

function M.is_stale(buf)
  local state = states[buf or current()]
  return state ~= nil and state.stale == true
end

-- Suitable for winbar/statusline expressions; exposes activity even with no slice.
function M.indicator(buf)
  local state = states[buf or current()]
  if not state then return "Flowistry: OFF" end
  if state.busy then
    return ("Flowistry: %s... %.1fs"):format(state.phase or "Analyzing", (uv.hrtime() - state.started) / 1e9)
  end
  local labels = {
    active = "ON", pinned = "PINNED", idle = "ON",
    ["waiting for save"] = "Save modified project files",
    ["outside function"] = "ON - move inside a function",
    ["no place"] = "ON - select a variable", editing = "ON - editing",
    ["pinned target unavailable"] = "Pinned target unavailable - undo, repin or :Flow unpin",
    ["analysis unavailable"] = "Analysis unavailable for this function",
    error = "ERROR - :Flowistry log",
  }
  return "Flowistry: " .. (labels[state.status] or state.status)
    .. (state.stale and " (showing saved analysis)" or "")
    .. (not state.stale and state.slice and state.cached and " (disk cache)" or "")
end

function M.log()
  local state = states[current()]
  local message = state and state.error or "No Flowistry error for this buffer."
  vim.cmd("botright new")
  local buf = current()
  vim.bo[buf].buftype, vim.bo[buf].bufhidden, vim.bo[buf].swapfile = "nofile", "wipe", false
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.split(message, "\n", { plain = true }))
  vim.bo[buf].modifiable = false
  vim.keymap.set("n", "q", "<Cmd>close<CR>", { buffer = buf, silent = true })
end

return M
