local M = {}

-- Each operation owns a cancellable chain of subprocesses. Never invoke a shell.
local function operation()
  local op = { cancelled = false }
  function op:cancel()
    self.cancelled = true
    if self.process then pcall(self.process.kill, self.process, 15) end
  end
  function op:run(argv, opts, callback)
    if self.cancelled then return end
    local ok, process = pcall(vim.system, argv, opts, function(result)
      vim.schedule(function()
        if not self.cancelled then callback(result) end
      end)
    end)
    if ok then
      self.process = process
    else
      vim.schedule(function()
        if not self.cancelled then callback({ code = -1, stderr = tostring(process), stdout = "" }) end
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
  if config.command then
    vim.schedule(function()
      if not op.cancelled then callback(nil, { root = root, env = env, command = vim.deepcopy(config.command) }) end
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
      callback(nil, {
        root = vim.fs.dirname(manifest), env = env,
        command = vim.list_extend(cargo, { "flowistry" }),
      })
    end)
  end)
  return op
end

function M.request(context, args, config, callback)
  local op = operation()
  local argv = vim.deepcopy(context.command)
  if config.context_mode then vim.list_extend(argv, { "--context-mode", config.context_mode }) end
  vim.list_extend(argv, args)
  local env = vim.deepcopy(context.env or {})
  if config.cache == false then env.FLOWISTRY_CACHE = "off" end
  if config.cache_dir then env.FLOWISTRY_CACHE_DIR = config.cache_dir end
  if config.cache_refresh and config.cache ~= false then env.FLOWISTRY_CACHE = "refresh" end
  op:run(argv, { cwd = context.root, env = env, text = true, timeout = config.timeout_ms }, function(result)
    if result.code ~= 0 then callback(table.concat(argv, " ") .. "\n" .. failure(result)); return end
    local encoded = (result.stdout or ""):gsub("%s", "")
    if encoded == "" and vim.trim(result.stderr or "") ~= "" then
      callback("Flowistry produced no analysis:\n" .. vim.trim(result.stderr)); return
    end
    local ok, compressed = pcall(vim.base64.decode, encoded)
    if not ok or compressed == "" then callback("Invalid base64 response from Flowistry"); return end
    op:run({ config.gzip, "-dc" }, { stdin = compressed, timeout = config.timeout_ms }, function(decoded)
      if decoded.code ~= 0 then callback("Cannot decompress Flowistry response: " .. failure(decoded)); return end
      local valid, value = pcall(vim.json.decode, decoded.stdout)
      if not valid or type(value) ~= "table" then callback("Invalid JSON response from Flowistry"); return end
      if value.Err ~= nil then
        local err = value.Err
        callback(type(err) == "table" and ((err.type or "AnalysisError") .. ": " .. (err.error or vim.inspect(err))) or tostring(err))
      elseif type(value.Ok) == "table" then
        callback(nil, value.Ok)
      else
        callback("Unrecognized Flowistry response (expected a Rust Result)")
      end
    end)
  end)
  return op
end

return M
