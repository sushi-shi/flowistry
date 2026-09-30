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
local background
local inputs = require("flowistry.inputs").new()
local input_epoch = 0
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
  parameter_types = true, -- Focusing an argument's type selects its binding.
  progress = false, -- Session launcher enables analysis progress popups.
  project = { enabled = false, idle_ms = 300, memory_mib = 6144, timeout_seconds = 600,
    max_workspaces = 1, max_body_bytes = 8 * 1024 * 1024, max_results_bytes = 16 * 1024 * 1024 },
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
  if background and states[buf] then background:detach(states[buf]) end
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
  local file = uv.fs_open(name, "r", 438)
  if not file then return false end
  local stat = uv.fs_fstat(file)
  -- Undo restoration is optional. Large/binary inputs must not create an
  -- unbounded second copy of every editor buffer merely to avoid revalidation.
  local text = stat and stat.type == "file" and stat.size <= 1024 * 1024
    and uv.fs_read(file, stat.size, 0) or false
  uv.fs_close(file)
  return text or false
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
      if vim.bo[buf].buftype == "" and inputs:matches(root, name) then return true end
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
  local observed_inputs = input_epoch
  state.busy, state.status = true, "loading"
  state.phase, state.started = phase or "Preparing compiler", uv.hrtime()
  vim.cmd("redrawstatus")
  return function(err, value)
    if states[state.buf] ~= state or state.epoch ~= epoch or not vim.api.nvim_buf_is_valid(state.buf) then return end
    if observed_inputs ~= input_epoch then
      invalidate(state, true)
      vim.schedule(function() if states[state.buf] == state then update(state) end end)
      return
    end
    if vim.api.nvim_buf_get_changedtick(state.buf) ~= tick or vim.api.nvim_buf_get_name(state.buf) ~= name then
      invalidate(state, true)
      state.status = "waiting for save"
      return
    end
    state.busy, state.operation = false, nil
    progress.close(state)
    state.last_ms = (uv.hrtime() - state.started) / 1e6
    if err then fail(state, err); return end
    if value._input_watch ~= nil then
      inputs:observe(state.context and state.context.root or state.root, value._input_watch, value._input_scope)
      if dirty(state.context and state.context.root or state.root) then
        invalidate(state, true); state.status = "waiting for save"; return
      end
    end
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
  result.comments = place_list(value.comments or {})
  result.parameter_aliases = {}
  for _, alias in ipairs(value.parameter_aliases or {}) do
    local range, target = place_range(alias.range), place_range(alias.target)
    if range and target then
      result.parameter_aliases[#result.parameter_aliases + 1] = { range = range, target = target }
    end
  end
  return result
end

local function new_background()
  return require("flowistry.background").new(config, {
    valid = function(state) return states[state.buf] == state and vim.api.nvim_buf_is_valid(state.buf) end,
    dirty = dirty,
    inputs = function(root, value, target, filename)
      inputs:observe(root, value, require("flowistry.inputs").scope(filename, target))
    end,
    revision = function() return input_epoch end,
    progress = function() vim.cmd("redrawstatus") end,
    result = function(state, value, event)
      -- A compiler result describes saved bytes. A changed disk file that has
      -- not yet been reloaded into this buffer must never color the old text.
      local ok, lines = pcall(vim.fn.readfile, vim.api.nvim_buf_get_name(state.buf))
      if not ok or not vim.deep_equal(lines, vim.api.nvim_buf_get_lines(state.buf, 0, -1, false)) then return end
      assert(type(value.bodies) == "table" and vim.islist(value.bodies), "Missing background bodies")
      local convert = ranges.converter(state.buf, state.context.root, value.bodies[1] and value.bodies[1].range.filename)
      state.bodies = state.bodies or {}
      for _, item in ipairs(value.bodies) do
        local range = convert(item.range)
        if range then
          local found
          for _, existing in ipairs(state.bodies) do if vim.deep_equal(existing.range, range) then found = existing; break end end
          if not found then found = { range = range }; state.bodies[#state.bodies + 1] = found end
          if item.focus ~= vim.NIL and item.focus then
            if not found.focus or found.background_identity then found.background_identity = event.body.identity end
            if item.focus.Ok then found.focus, found.error = prepare_focus(state, item.focus.Ok), nil
            elseif item.focus.Err then found.focus, found.error = nil, item.focus.Err
            else error("Invalid background body result") end
            found.cached = item.cached == true
          end
        end
      end
      if current() == state.buf and not state.busy then update(state) end
    end,
    evict = function(state, identity)
      for _, bodies in ipairs({ state.bodies or {}, state.retained and state.retained.bodies or {} }) do
        for _, body in ipairs(bodies) do
          if body.background_identity == identity then body.focus, body.background_identity = nil, nil end
        end
      end
    end,
  })
end

local function request(state, args, request_config, handler)
  if not config.project.enabled then return backend.request(state.context, args, request_config, handler) end
  request_config = vim.tbl_extend("force", request_config, { selection = background:selection(state) })
  return background:foreground(state.context, backend.key(state.context, args, request_config), function(done)
    return backend.request(state.context, args, request_config, done)
  end, handler)
end

update = function(state)
  if states[state.buf] ~= state or current() ~= state.buf then return end
  -- Saving from Insert mode can let rustfmt replace whole lines and displace
  -- extmarks. Finish that save's redraw without resuming ordinary cursor
  -- tracking while the user is typing.
  if vim.api.nvim_get_mode().mode:sub(1, 1) == "i" and not state.refresh_after_save then
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
    state.refresh_after_save = nil
    return
  end
  if not state.context then
    state.operation = backend.context(state.root, config, callback(state, function(context)
      state.context = context
      if background then background:attach(state) end
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
      state.operation = request(state, args, request_config, callback(state, function(value)
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
    state.operation = request(state, { "spans", filename }, config, callback(state, function(value)
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
    state.refresh_after_save = nil
    return
  end
  if not body.focus then
    if not state.stale then render.clear(state.buf) end
    if body.error then
      state.slice, state.status = nil, "analysis unavailable"
      state.refresh_after_save = nil
      return
    end
    -- Request the body we selected, rather than the cursor point. rustc's
    -- zero-width span containment includes a closure's end, while editor
    -- ranges are half-open: at that boundary the cursor selects its parent.
    local request_pos = config.batch and body.range.start or pos
    local char = ranges.position(state.buf, { request_pos[1] + 1, request_pos[2] })
    state.operation = request(state, {
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
            body.background_identity = nil
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
    state.buf, body.focus, pos, config.priority, config.show_influence, config.show_maybe, config.parameter_types
  )
  state.cached = body.cached == true
  state.stale = false
  state.status = state.slice and (state.mark and "pinned" or "active") or "no place"
  state.refresh_after_save = nil
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
  if background then background:close() end
  for _, state in pairs(all_states()) do stop(state); clear_pin(state); render.clear(state.buf) end
  states, suspended, disabled = {}, {}, {}
  inputs, input_epoch = require("flowistry.inputs").new(), 0
  local packaged_ok, packaged = pcall(require, "flowistry.packaged")
  config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), packaged_ok and packaged or {}, opts or {})
  assert(type(config.debounce_ms) == "number" and config.debounce_ms >= 0, "debounce_ms must be nonnegative")
  assert(type(config.timeout_ms) == "number" and config.timeout_ms > 0, "timeout_ms must be positive")
  assert(type(config.auto_enable) == "boolean", "auto_enable must be a boolean")
  assert(type(config.cache) == "boolean", "cache must be a boolean")
  assert(config.cache_dir == nil or (type(config.cache_dir) == "string" and config.cache_dir ~= ""), "cache_dir must be a nonempty path")
  assert(not config.command or (vim.islist(config.command) and #config.command > 0), "command must be an argv list")
  assert(type(config.project) == "table" and type(config.project.enabled) == "boolean", "project.enabled must be a boolean")
  assert(not config.project.enabled or config.cache, "project background analysis requires cache=true")
  for _, name in ipairs({ "idle_ms", "memory_mib", "timeout_seconds", "max_workspaces", "max_body_bytes", "max_results_bytes" }) do
    assert(type(config.project[name]) == "number" and config.project[name] > 0, "project." .. name .. " must be positive")
  end
  background = new_background()
  render.highlights()
  local group = vim.api.nvim_create_augroup("Flowistry", { clear = true })
  local observed_ticks, observed_names, saved_sources = {}, {}, {}
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) then
      observed_ticks[buf] = vim.api.nvim_buf_get_changedtick(buf)
      observed_names[buf] = vim.api.nvim_buf_get_name(buf)
      local name = observed_names[buf]
      if name ~= "" and vim.bo[buf].buftype == "" then
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
        if name ~= "" and vim.bo[args.buf].buftype == "" then
          saved_sources[name] = disk_source(name)
        end
        return
      end
      -- TextChanged may be delivered when entering a buffer even though no edit
      -- happened since its read/write or the last observed change.
      if args.event:match("^TextChanged") and (previous_tick == tick
        or (previous_tick == nil and not vim.bo[args.buf].modified)) then return end
      if name == "" or vim.bo[args.buf].buftype ~= "" then return end
      -- Any on-disk input can matter (includes, build scripts, Cargo config,
      -- path dependencies). Pending discovery also rejects intervening edits.
      input_epoch = input_epoch + 1
      local invalidated = {}
      for _, state in pairs(all_states()) do
        local root = state.context and state.context.root or state.root
        if state.buf == args.buf or inputs:matches(root, name)
          or (args.event == "BufFilePost" and previous_name and inputs:matches(root, previous_name)) then
          invalidate(state, args.event ~= "BufReadPost" and args.event ~= "BufFilePost")
          if not invalidated[root or false] then
            background:invalidate(root, args.event == "BufWritePost" and name or nil)
            invalidated[root or false] = true
          end
          if name:match("Cargo%.toml$") or name:match("Cargo%.lock$")
            or name:match("/%.cargo/config%.toml$") or name:match("/%.cargo/config$")
            or name:match("/rust%-toolchain$") or name:match("/rust%-toolchain%.toml$") then
            state.context = nil
            background:rediscover(root)
          end
          if args.event == "BufWritePost" then state.refresh_after_save = true end
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
  vim.api.nvim_create_autocmd("FocusGained", {
    group = group,
    callback = function()
      input_epoch = input_epoch + 1
      local roots = {}
      for _, state in pairs(all_states()) do
        local root = state.context and state.context.root or state.root
        invalidate(state, true)
        -- Changes outside Neovim have no complete editor event history.
        state.retained = nil
        state.context = nil
        if root and not roots[root] then background:rediscover(root); roots[root] = true end
      end
      if states[current()] then schedule(states[current()]) end
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
      background:close()
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
  if state.context then background:attach(state) end
  update(state)
  if state.status == "waiting for save" and not quiet then notify("Save modified Rust buffers in this project to analyze them.") end
end

function M.disable(buf)
  buf = buf or current()
  disabled[buf] = true
  local state = states[buf]
  if state then
    background:detach(state)
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
  local body = ranges.smallest(state.bodies or {}, pos, function(item) return item.range end)
  if body and body.focus then
    pos = render.selection(body.focus, pos, config.parameter_types)
    if not pos then return end
  end
  if not ranges.token(state.buf, pos) then return end
  if state.mark and pins.contains(state.buf, state.mark, pos) then
    clear_pin(state)
  else
    state.mark = pins.set(state.buf, pos, state.mark)
  end
  update(state)
end

function M.types()
  config.parameter_types = config.parameter_types == false
  if states[current()] then update(states[current()]) end
  notify("argument type selection " .. (config.parameter_types and "enabled" or "disabled"))
end

function M.unmark()
  local state = states[current()]
  if state then clear_pin(state); update(state) end
end

function M.refresh()
  -- Also refresh sibling buffers whose dependencies may have changed on disk.
  for _, state in pairs(all_states()) do
    background:rediscover(state.context and state.context.root or state.root)
    invalidate(state)
    state.context, state.cache_stats, state.cache_refresh = nil, nil, true
  end
  if states[current()] then update(states[current()]) else M.enable() end
end

function M.project()
  if not configured then M.setup() end
  if not config.project.enabled and not config.cache then
    notify("Enable the shared cache before starting project background analysis.", vim.log.levels.WARN); return
  end
  config.project.enabled = not config.project.enabled
  background:close()
  for _, state in pairs(all_states()) do invalidate(state, true); state.context = nil end
  background = new_background()
  if states[current()] then update(states[current()]) else M.enable() end
  notify("project background analysis " .. (config.project.enabled and "enabled" or "disabled"))
end

function M.stop()
  if not configured then M.setup({ auto_enable = false }) end
  config.auto_enable = false
  for buf in pairs(states) do M.disable(buf) end
  background:close()
  background = new_background()
end

function M.start()
  if not configured then M.setup() end
  config.auto_enable, disabled = true, {}
  M.enable(true)
end

function M.project_status(buf)
  local state = states[buf or current()]
  return state and background:status(state.context and state.context.root or state.root) or nil
end

function M.cache_status(buf)
  local state = states[buf or current()]
  return state and state.cache_stats and vim.deepcopy(state.cache_stats) or nil
end

function M.input_status(buf)
  local state = states[buf or current()]
  return state and inputs:status(state.context and state.context.root or state.root) or nil
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
  local project = M.project_status(buf)
  local work = project and project.status == "running" and
    (" (project %d/%s)"):format(project.completed, project.total or "?") or ""
  if project and (project.failed > 0 or project.status == "unavailable") then work = work .. " (project incomplete)" end
  return "Flowistry: " .. (labels[state.status] or state.status)
    .. (state.stale and " (showing saved analysis)" or "")
    .. (not state.stale and state.slice and state.cached and " (disk cache)" or "")
    .. work
end

function M.log()
  local state = states[current()]
  local message = state and state.error or "No Flowistry error for this buffer."
  local project = M.project_status()
  if project and project.error then message = message .. "\nProject analysis:\n" .. project.error end
  local watched = M.input_status()
  if watched then message = message .. "\nInput invalidation:\n" .. vim.inspect(watched) end
  vim.cmd("botright new")
  local buf = current()
  vim.bo[buf].buftype, vim.bo[buf].bufhidden, vim.bo[buf].swapfile = "nofile", "wipe", false
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.split(message, "\n", { plain = true }))
  vim.bo[buf].modifiable = false
  vim.keymap.set("n", "q", "<Cmd>close<CR>", { buffer = buf, silent = true })
end

return M
