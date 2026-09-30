local M = {}
local backend = require("flowistry.backend")
local uv = vim.uv or vim.loop

local function canonical(path) return uv.fs_realpath(path) or vim.fs.normalize(path) end
local function contains(path, root) return path == root or path:sub(1, #root + 1) == root .. "/" end
local function key(target) return table.concat({ target.package_id or target.package, target.target_kind, target.target_name }, "\0") end

-- One queue per canonical Cargo workspace. Foreground requests own a slot before
-- cancellation starts, so a resumed background job cannot overtake them.
function M.new(config, hooks)
  local self = { workspaces = {}, closed = false, retained = {}, retained_bytes = 0 }
  local schedule, dispatch, pump

  local function workspace(context)
    local root = canonical(context.root)
    local w = self.workspaces[root]
    if not w then
      w = { root = root, context = context, states = {}, epoch = 0, targets = {}, next_target = 1,
        foreground = {}, requests = {}, pending = {}, status = "idle", done = false }
      self.workspaces[root] = w
    end
    return w
  end

  local function changed(w)
    if hooks.progress then hooks.progress(w) end
  end

  local function stop_timer(w)
    if w.timer then w.timer:stop(); if not w.timer:is_closing() then w.timer:close() end; w.timer = nil end
  end

  local function stop(w)
    stop_timer(w)
    w.epoch = w.epoch + 1
    if w.decode then w.decode:cancel(); w.decode = nil end
    w.delivery = nil
    if w.operation then w.operation:cancel() end
  end

  local function forget(state)
    for id, item in pairs(self.retained) do
      if item.state == state then
        if hooks.evict then hooks.evict(state, item.identity) end
        self.retained_bytes = self.retained_bytes - item.bytes; self.retained[id] = nil
      end
    end
  end

  local function retain(state, event, bytes)
    local limit = config.project.max_results_bytes or 16 * 1024 * 1024
    if bytes > limit then return nil end
    local id = tostring(state.buf) .. "\0" .. event.body.identity
    local previous = self.retained[id]
    if previous then self.retained_bytes = self.retained_bytes - previous.bytes; self.retained[id] = nil end
    while self.retained_bytes + bytes > limit do
      local oldest, chosen
      for candidate, item in pairs(self.retained) do
        if not chosen or item.used < chosen.used then oldest, chosen = candidate, item end
      end
      if not chosen then return nil end
      if hooks.evict then hooks.evict(chosen.state, chosen.identity) end
      self.retained_bytes = self.retained_bytes - chosen.bytes
      self.retained[oldest] = nil
    end
    local item = { state = state, identity = event.body.identity, bytes = bytes, used = uv.hrtime() }
    self.retained[id] = item
    self.retained_bytes = self.retained_bytes + bytes
    return true
  end

  local function targets(w)
    local result = {}
    local requested = config.project.targets
    if type(requested) == "function" then requested = requested(w.root) end
    for _, target in ipairs(w.context.targets or {}) do
      local include = target.target_kind == "lib" or target.target_kind == "bin"
      if requested then
        include = false
        for _, wanted in ipairs(requested) do
          if (target.package == wanted.package or target.package_id == wanted.package)
            and target.target_kind == wanted.target_kind and target.target_name == wanted.target_name then include = true end
        end
      end
      -- Cargo targets requiring opt-in features are left for an explicit target
      -- configuration; background discovery must not silently turn on features.
      if not requested and #(target.required_features or {}) > 0 and not config.project.all_features then include = false end
      if include and target.supported then result[#result + 1] = target end
    end
    return result
  end

  local function preferred(w, filename)
    local best, score
    for _, target in ipairs(w.targets) do
      local src, root = canonical(target.src_path), canonical(vim.fs.dirname(target.manifest_path))
      local rank = src == filename and (1000000 + #root)
        or (contains(filename, root) and (#root * 10 + (target.target_kind == "lib" and 1 or 0)) or -1)
      if not score or rank > score or (rank == score and key(target) < key(best)) then best, score = target, rank end
    end
    return score and score >= 0 and best or (#w.targets == 1 and w.targets[1] or nil)
  end

  local function active(w)
    local state = w.states[vim.api.nvim_get_current_buf()]
    return state and hooks.valid(state) and state or nil
  end

  local function order(w)
    local state = active(w)
    if not state then return end
    local wanted = preferred(w, canonical(vim.api.nvim_buf_get_name(state.buf)))
    if not wanted then return end
    for i = w.next_target, #w.targets do
      if key(w.targets[i]) == key(wanted) then
        table.remove(w.targets, i); table.insert(w.targets, w.next_target, wanted); return
      end
    end
  end

  local function deliver(w, event, target, epoch, ticks)
    if event.event ~= "body" or not event.current or type(event.output) ~= "string" then return end
    for buf, state in pairs(w.states) do
      if hooks.valid(state) and vim.api.nvim_buf_is_loaded(buf)
        and canonical(vim.api.nvim_buf_get_name(buf)) == canonical(event.body.range.filename)
        and preferred(w, canonical(event.body.range.filename)) == target
        and vim.api.nvim_buf_get_changedtick(buf) == ticks[buf] and not vim.bo[buf].modified then
        -- Only open buffers need a decoded object. Other results remain solely
        -- in the backend's bounded shared store. Coalesce slow decode consumers.
        w.delivery = { state = state, event = event, epoch = epoch, tick = ticks[buf] }
        break
      end
    end
    local function next_delivery()
      if w.decode or not w.delivery then return end
      local item = w.delivery
      w.delivery = nil
      local decoder
      decoder = backend.decode(item.event.output, vim.tbl_extend("force", config, {
        decode_limit = config.project.max_body_bytes or 8 * 1024 * 1024,
      }), function(err, value, bytes)
        if w.decode ~= decoder then return end
        w.decode = nil
        local state = item.state
        if not err and w.epoch == item.epoch and hooks.valid(state)
          and vim.api.nvim_buf_get_changedtick(state.buf) == item.tick and not vim.bo[state.buf].modified
          and not hooks.dirty(w.root) then
          if not retain(state, item.event, bytes or 0) then next_delivery(); return end
          local ok, reason = pcall(hooks.result, state, value, item.event)
          if not ok then w.error = tostring(reason); changed(w) end
        elseif err and w.epoch == item.epoch then w.error = err; changed(w) end
        next_delivery()
      end)
      w.decode = decoder
    end
    next_delivery()
  end

  dispatch = function(w)
    if self.closed or not config.project.enabled or next(w.foreground) or w.operation
      or not next(w.states) or hooks.dirty(w.root) or w.done or w.needs_metadata then return end
    order(w)
    local target = w.targets[w.next_target]
    if not target then w.done, w.status = true, (w.failed or 0) > 0 and "partial" or "complete"; changed(w); return end
    local epoch, ticks = w.epoch, {}
    local args = { "--package", target.package_id or target.package, "--target-kind", target.target_kind, "--target-name", target.target_name }
    if config.project.features then vim.list_extend(args, { "--features", config.project.features }) end
    if config.project.all_features then args[#args + 1] = "--all-features" end
    if config.project.no_default_features then args[#args + 1] = "--no-default-features" end
    vim.list_extend(args, { "project", "--stream", "ndjson-v1", "--memory-mib", tostring(config.project.memory_mib),
      "--timeout-seconds", tostring(config.project.timeout_seconds) })
    local state = active(w)
    if state then
      local pos = require("flowistry.ranges").position(state.buf, vim.api.nvim_win_get_cursor(0))
      vim.list_extend(args, { "--cursor-file", vim.api.nvim_buf_get_name(state.buf),
        "--cursor-line", tostring(pos[1]), "--cursor-column", tostring(pos[2]) })
    end
    for buf, item in pairs(w.states) do
      if hooks.valid(item) then
        ticks[buf] = vim.api.nvim_buf_get_changedtick(buf)
        vim.list_extend(args, { "--priority-file", vim.api.nvim_buf_get_name(buf) })
      end
    end
    w.status, w.completed, w.total, w.target = "running", 0, nil, target
    changed(w)
    w.operation = backend.stream(w.context, args, config, function(event)
      if w.epoch ~= epoch or self.closed then return end
      if event.event == "inventory" then w.total = event.total end
      if event.event == "body" then
        w.completed = w.completed + 1
        if event.status ~= "current" and event.status ~= "uncached" then
          w.failed = (w.failed or 0) + 1
          w.error = (event.body.name or event.body.identity) .. ": " .. tostring(event.status)
            .. (event.diagnostics and ("\n" .. event.diagnostics:sub(1, 65536)) or "")
        end
        deliver(w, event, target, epoch, ticks)
      end
      if event.event == "diagnostic" then w.error = event.message end
      if event.event == "finished" then
        w.status = event.status
        if event.status == "complete" or event.status == "partial" then w.next_target = w.next_target + 1 end
        if event.status == "failed" then
          w.error = w.error or "Project analysis failed"; w.next_target = w.next_target + 1; w.failed = (w.failed or 0) + 1
        end
      end
      changed(w)
    end, function(err, result)
      w.operation = nil
      if w.epoch == epoch and err then
        w.error, w.status, w.next_target = err, "error", w.next_target + 1
        w.failed = (w.failed or 0) + 1
      end
      if result.cancelled then w.status = "paused" end
      changed(w)
      pump(w)
      schedule(w)
    end)
  end

  schedule = function(w)
    stop_timer(w)
    if self.closed or not config.project.enabled or next(w.foreground) or w.operation or w.done then return end
    w.timer = vim.defer_fn(function() w.timer = nil; dispatch(w) end, config.project.idle_ms)
  end

  pump = function(w)
    if w.operation then return end
    local queue = w.pending
    w.pending = {}
    for _, start in ipairs(queue) do start() end
  end

  function self:attach(state)
    if self.closed or not config.project.enabled or not state.context.targets then return end
    local w = workspace(state.context)
    w.context = state.context
    w.needs_metadata = false
    if state.background_root and state.background_root ~= w.root then self:detach(state) end
    state.background_root, w.states[state.buf] = w.root, state
    if #w.targets == 0 then
      w.targets = targets(w); w.done = false
      if #w.targets == 0 then w.done, w.status, w.error = true, "unavailable", "No supported targets match the background configuration" end
    end
    schedule(w)
  end

  function self:foreground(context, request_key, launch, callback)
    local w = workspace(context)
    local op = { cancelled = false }
    local released = false
    w.foreground[op] = true
    local group = w.requests[request_key]
    local joined = group ~= nil
    if not group then
      group = { listeners = {}, finalized = false }
      w.requests[request_key] = group
    end
    local function release()
      if released then return end
      released = true; w.foreground[op] = nil; schedule(w)
    end
    group.listeners[op] = { callback = callback, release = release }
    function op:cancel()
      self.cancelled = true
      group.listeners[self] = nil
      if not next(group.listeners) and not group.finalized then
        group.finalized = true
        w.requests[request_key] = nil
        if group.child then group.child:cancel() end
      end
      release()
    end
    if joined then return op end
    local function begin()
      if group.finalized or self.closed then return end
      group.child = launch(function(err, value)
        if group.finalized then return end
        group.finalized = true; w.requests[request_key] = nil
        local listeners = group.listeners
        group.listeners = {}
        for _, listener in pairs(listeners) do listener.release() end
        for listener_op, listener in pairs(listeners) do
          if not listener_op.cancelled then listener.callback(err, value) end
        end
      end)
    end
    w.pending[#w.pending + 1] = begin
    stop(w)
    pump(w)
    return op
  end

  function self:invalidate(root)
    if not root then return end
    local w = self.workspaces[canonical(root)]
    if w then
      stop(w); w.next_target, w.done, w.status = 1, false, "waiting for save"
      w.failed, w.error = 0, nil
      for _, state in pairs(w.states) do forget(state) end
      schedule(w); changed(w)
    end
  end

  function self:selection(state)
    local w = state.background_root and self.workspaces[state.background_root]
    return w and preferred(w, canonical(vim.api.nvim_buf_get_name(state.buf))) or nil
  end

  function self:rediscover(root)
    local w = root and self.workspaces[canonical(root)]
    if w then self:invalidate(root); w.targets = {}; w.needs_metadata = true end
  end

  function self:detach(state)
    forget(state)
    local w = state.background_root and self.workspaces[state.background_root]
    if w then
      w.states[state.buf] = nil
      if not next(w.states) then stop(w) end
    end
    state.background_root = nil
  end

  function self:close()
    self.closed = true
    for _, w in pairs(self.workspaces) do
      stop(w)
      for op in pairs(w.foreground) do op:cancel() end
      pump(w)
    end
  end

  function self:status(root)
    local w = root and self.workspaces[canonical(root)]
    if not w or not config.project.enabled then return nil end
    return { status = w.status, completed = w.completed or 0, total = w.total,
      failed = w.failed or 0,
      target = w.target and w.target.target_name, error = w.error,
      retained_result_bytes = self.retained_bytes,
      targets_completed = math.min(w.next_target - 1, #w.targets), targets_total = #w.targets }
  end

  return self
end

return M
