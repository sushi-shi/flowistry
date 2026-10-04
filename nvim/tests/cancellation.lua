-- Real foreground process trees must stop on both cancellation and timeout.
local uv = vim.uv or vim.loop
local script = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p')
if arg[1] == 'worker' then
  local root, role = arg[2], arg[3]
  vim.fn.writefile({ tostring(uv.os_getpid()) }, root .. '/' .. role)
  if role == 'parent' then
    vim.system({ vim.v.progpath, '--headless', '-u', 'NONE', '-i', 'NONE', '-l', script, 'worker', root, 'child' },
      { stdout = false, stderr = false })
  end
  vim.wait(30000, function() return false end, 20)
  vim.cmd('qa!')
  return
end

vim.opt.rtp:prepend(vim.fn.getcwd())
local backend = require('flowistry.backend')
local function alive(pid)
  -- A killed orphan may briefly remain as a zombie awaiting its reaper.
  local file = io.open('/proc/' .. pid .. '/stat')
  if not file then return false end
  local stat = file:read('*a'); file:close()
  -- The process can disappear between opening procfs and reading the file.
  return stat ~= nil and not stat:match('%) Z ')
end
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local active = {}
local ok, err = xpcall(function()
  for _, mode in ipairs({ 'cancel', 'timeout' }) do
    local root = vim.fn.tempname(); vim.fn.mkdir(root, 'p')
    local done, error
    local op = backend.request({ root = root, env = {}, command = {
      vim.v.progpath, '--headless', '-u', 'NONE', '-i', 'NONE', '-l', script, 'worker', root, 'parent' } },
      {}, { timeout_ms = mode == 'timeout' and 2000 or 10000, gzip = 'gzip' },
      function(e) done, error = true, e end)
    active[#active + 1] = op
    check(vim.wait(1500, function() return vim.fn.filereadable(root .. '/child') == 1 end, 10), 'child did not start')
    local parent, child = tonumber(vim.fn.readfile(root .. '/parent')[1]), tonumber(vim.fn.readfile(root .. '/child')[1])
    if mode == 'cancel' then op:cancel() end
    check(vim.wait(5000, function() return not alive(parent) and not alive(child) end, 10), mode .. ' left descendants running')
    vim.wait(50, function() return false end, 10)
    if mode == 'cancel' then check(not done, 'canceled callback delivered')
    else check(done and error:find('124', 1, true), 'timeout was not delivered as code 124') end
  end
end, debug.traceback)
for _, op in ipairs(active) do op:cancel() end
if not ok then io.stderr:write(err .. '\n'); vim.cmd('cquit 1') end
print(('Flowistry cancellation: %d assertions passed'):format(passed))
vim.cmd('qa!')
