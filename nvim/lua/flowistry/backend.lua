local M = {}
local uv = vim.uv or vim.loop

-- Each operation owns a cancellable chain of subprocesses. Never invoke a shell.
local function operation()
  local op = { cancelled = false }
  function op:cancel()
    self.cancelled = true
    if self.child then self.child:cancel() end
    if self.terminate then self.terminate() end
  end
  function op:run(argv, opts, callback)
    if self.cancelled then return end
    -- Cargo, the snapshot launcher and rustc are one foreground job. Give it
    -- a private process group so cancellation also releases descendant locks.
    local grouped = uv.os_uname().sysname ~= "Windows_NT"
    opts = vim.tbl_extend("force", opts, { detach = grouped })
    local timeout = opts.timeout
    opts.timeout = nil -- vim.system's timeout only kills the immediate process.
    local process, timer, finished, timed_out
    local function terminate()
      if finished or not process then return end
      if grouped then pcall(uv.kill, -process.pid, 9)
      else pcall(process.kill, process, 9) end
    end
    self.terminate = terminate
    local ok, spawned = pcall(vim.system, argv, opts, function(result)
      finished = true
      if timer then timer:stop(); timer:close(); timer = nil end
      if timed_out then result.code = 124 end
      vim.schedule(function()
        if not self.cancelled then callback(result) end
      end)
    end)
    if ok then
      process = spawned
      self.process = process
      if timeout and not finished then
        timer = vim.defer_fn(function() timer = nil; timed_out = true; terminate() end, timeout)
      end
    else
      finished = true
      vim.schedule(function()
        if not self.cancelled then callback({ code = -1, stderr = tostring(spawned), stdout = "" }) end
      end)
    end
  end
  return op
end

local function failure(result)
  local detail = vim.trim(result.stderr or "")
  if detail == "" then detail = vim.trim(result.stdout or "") end
  return ("Process exited with code %s%s"):format(result.code, detail ~= "" and (":\n" .. detail) or "")
end

local function command(config, executable)
  local argv = { executable }
  if config.toolchain then argv[#argv + 1] = "+" .. config.toolchain end
  return argv
end

function M.context(root, config, callback)
  local op = operation()
  local env = vim.deepcopy(config.env)
  local function ready(context)
    if not (config.project and config.project.enabled) then callback(nil, context); return end
    op.child = M.request(context, { "project-targets" }, config, function(err, metadata)
      if op.cancelled then return end
      if err then callback(err); return end
      if metadata.schema ~= 1 or type(metadata.workspace_root) ~= "string" or type(metadata.targets) ~= "table" then
        callback("Invalid project target inventory"); return
      end
      context.root, context.targets = metadata.workspace_root, metadata.targets
      callback(nil, context)
    end)
  end
  if config.command then
    vim.schedule(function()
      if not op.cancelled then ready({ root = root, env = env, command = vim.deepcopy(config.command) }) end
    end)
    return op
  end
  local argv = command(config, "rustc")
  vim.list_extend(argv, { "--print", "target-libdir", "--print", "sysroot" })
  op:run(argv, { cwd = root, env = env, text = true, timeout = config.timeout_ms }, function(result)
    if result.code ~= 0 then callback(table.concat(argv, " ") .. "\n" .. failure(result)); return end
    local paths = vim.split(vim.trim(result.stdout), "\n", { plain = true })
    if #paths ~= 2 then callback("Could not determine Flowistry's compiler sysroot"); return end
    local system = (vim.uv or vim.loop).os_uname().sysname
    local key = system == "Darwin" and "DYLD_LIBRARY_PATH" or (system == "Windows_NT" and "PATH" or "LD_LIBRARY_PATH")
    local sep = system == "Windows_NT" and ";" or ":"
    local inherited = env[key] or vim.env[key]
    env[key] = paths[1] .. (inherited and (sep .. inherited) or "")
    env.SYSROOT = paths[2]
    env.RUSTUP_TOOLCHAIN = config.toolchain or env.RUSTUP_TOOLCHAIN
    local cargo = command(config, "cargo")
    local locate = vim.list_extend(vim.deepcopy(cargo), { "locate-project", "--workspace", "--message-format", "plain" })
    op:run(locate, { cwd = root, env = env, text = true, timeout = config.timeout_ms }, function(located)
      if located.code ~= 0 then callback(failure(located)); return end
      local manifest = vim.trim(located.stdout)
      if not manifest:match("Cargo%.toml$") then callback("Cargo returned an invalid workspace manifest"); return end
      ready({
        root = vim.fs.dirname(manifest), env = env,
        command = vim.list_extend(cargo, { "flowistry" }),
      })
    end)
  end)
  return op
end

local function invocation(context, args, config)
  local argv = vim.deepcopy(context.command)
  if config.selection then
    local target = config.selection
    vim.list_extend(argv, { "--package", target.package_id or target.package, "--target-kind", target.target_kind, "--target-name", target.target_name })
    if config.project.features then vim.list_extend(argv, { "--features", config.project.features }) end
    if config.project.all_features then argv[#argv + 1] = "--all-features" end
    if config.project.no_default_features then argv[#argv + 1] = "--no-default-features" end
    vim.list_extend(argv, { "--context-mode", config.context_mode or "SigOnly",
      "--mutability-mode", "DistinguishMut", "--pointer-mode", "Precise" })
  elseif config.context_mode then vim.list_extend(argv, { "--context-mode", config.context_mode }) end
  vim.list_extend(argv, args)
  local env = vim.deepcopy(context.env or {})
  if config.cache == false then env.FLOWISTRY_CACHE = "off" end
  if config.cache_dir then env.FLOWISTRY_CACHE_DIR = config.cache_dir end
  if config.cache_refresh and config.cache ~= false then env.FLOWISTRY_CACHE = "refresh" end
  return argv, env
end

function M.key(context, args, config)
  local argv, env = invocation(context, args, config)
  local environment = {}
  for name, value in pairs(env) do environment[#environment + 1] = { name, value } end
  table.sort(environment, function(a, b) return a[1] < b[1] end)
  return vim.json.encode({ context.root, argv, environment })
end

local function decode(op, encoded, config, callback)
  local ok, compressed = pcall(vim.base64.decode, encoded:gsub("%s", ""))
  if not ok or compressed == "" then callback("Invalid base64 response from Flowistry"); return end
  local options = { stdin = compressed, timeout = config.timeout_ms }
  local chunks, length, oversized = {}, 0, false
  if config.decode_limit then
    options.stdout = function(_, data)
      if not data or oversized then return end
      length = length + #data
      if length > config.decode_limit then
        oversized = true
        vim.schedule(function() if op.process then pcall(op.process.kill, op.process, 15) end end)
      else chunks[#chunks + 1] = data end
    end
  end
  op:run({ config.gzip, "-dc" }, options, function(decoded)
    if oversized then callback("Background body exceeds the editor decode budget"); return end
    if config.decode_limit then decoded.stdout = table.concat(chunks) end
    if decoded.code ~= 0 then callback("Cannot decompress Flowistry response: " .. failure(decoded)); return end
    local valid, value = pcall(vim.json.decode, decoded.stdout)
    if not valid or type(value) ~= "table" then callback("Invalid JSON response from Flowistry"); return end
    if value.Err ~= nil then
      local err = value.Err
      callback(type(err) == "table" and ((err.type or "AnalysisError") .. ": " .. (err.error or vim.inspect(err))) or tostring(err))
    elseif type(value.Ok) == "table" then
      callback(nil, value.Ok, #decoded.stdout)
    else
      callback("Unrecognized Flowistry response (expected a Rust Result)")
    end
  end)
end

function M.decode(encoded, config, callback)
  local op = operation()
  decode(op, encoded, config, callback)
  return op
end

function M.request(context, args, config, callback)
  local op = operation()
  local argv, env = invocation(context, args, config)
  op:run(argv, { cwd = context.root, env = env, text = true, timeout = config.timeout_ms }, function(result)
    if result.code ~= 0 then callback(table.concat(argv, " ") .. "\n" .. failure(result)); return end
    local encoded = (result.stdout or ""):gsub("%s", "")
    if encoded == "" and vim.trim(result.stderr or "") ~= "" then
      callback("Flowistry produced no analysis:\n" .. vim.trim(result.stderr)); return
    end
    decode(op, encoded, config, callback)
  end)
  return op
end

-- Keep cancellation completion observable: the scheduler waits for the old
-- coordinator to exit before dispatching foreground work into its target lock.
function M.stream(context, args, config, on_event, on_done)
  local argv, env = invocation(context, args, config)
  local op = { cancelled = false }
  local pending, scheduled, exited, done = {}, false, nil, false
  local bytes, partial, stderr, problem, overflowed = 0, "", "", nil, false
  local sequence, run, finished = 0, nil, false
  local limit = 64 * 1024 * 1024
  function op:cancel()
    if self.cancelled then return end
    self.cancelled = true
    pending, bytes, partial = {}, 0, ""
    if self.process then pcall(self.process.kill, self.process, 15) end
    vim.defer_fn(function()
      if not exited and self.process then pcall(self.process.kill, self.process, 9) end
    end, 2000)
  end
  local drain
  local function schedule()
    if not scheduled then scheduled = true; vim.schedule(drain) end
  end
  local function reject(message)
    problem = message
    op:cancel()
  end
  drain = function()
    scheduled = false
    if done then return end
    local chunk = table.remove(pending, 1)
    if chunk and not op.cancelled then
      bytes = bytes - #chunk
      partial = partial .. chunk
      while true do
        local ending = partial:find("\n", 1, true)
        if not ending then break end
        local line = partial:sub(1, ending - 1)
        partial = partial:sub(ending + 1)
        local valid, event = pcall(vim.json.decode, line)
        if not valid or type(event) ~= "table" or event.schema ~= 1
          or event.sequence ~= sequence or type(event.run) ~= "string"
          or (run and event.run ~= run) or finished
          or not vim.tbl_contains({ "started", "inventory", "body-started", "body", "diagnostic", "finished" }, event.event)
          or (sequence == 0 and event.event ~= "started") or (sequence > 0 and event.event == "started")
          or (event.event == "finished" and not vim.tbl_contains({ "complete", "partial", "canceled", "failed" }, event.status)) then
          reject("Invalid project event stream"); break
        end
        run, sequence = event.run, sequence + 1
        finished = event.event == "finished"
        local ok, error = pcall(on_event, event)
        if not ok then reject("Invalid project event: " .. tostring(error)); break end
      end
    end
    if #pending > 0 then schedule(); return end
    if exited then
      done = true
      if not op.cancelled and (partial ~= "" or not finished) then
        problem = "Incomplete project event stream" .. (stderr ~= "" and (": " .. stderr) or "")
      end
      if not op.cancelled and exited.code ~= 0 and exited.code ~= 1 then problem = problem or failure(exited) end
      on_done(problem, { code = exited.code, cancelled = op.cancelled, stderr = stderr })
    end
  end
  local ok, process = pcall(vim.system, argv, { cwd = context.root, env = env,
    stdout = function(err, data)
      if op.cancelled or overflowed then return end
      if err then vim.schedule(function() reject(tostring(err)); schedule() end); return end
      if data then
        bytes = bytes + #data
        if bytes + #partial > limit then
          overflowed = true
          vim.schedule(function() reject("Project output exceeded the editor queue budget"); schedule() end)
          return
        end
        pending[#pending + 1] = data
        schedule()
      end
    end,
    stderr = function(_, data) if data then stderr = (stderr .. data):sub(1, 1024 * 1024) end end,
  }, function(result) exited = result; schedule() end)
  if ok then op.process = process else
    exited = { code = -1, stderr = tostring(process) }
    problem = tostring(process)
    schedule()
  end
  return op
end

return M
