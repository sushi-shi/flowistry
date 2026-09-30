-- Alternate baseline/current compiler requests; report every sample and median.
-- FILE ROW COLUMN [REPEATS]; FLOWISTRY_BASELINE_EXE and FLOWISTRY_BACKEND_EXE required.
vim.opt.rtp:prepend(vim.fn.getcwd())
local backend = require("flowistry.backend")
local uv = vim.uv or vim.loop
local file = vim.fn.fnamemodify(assert(arg[1]), ":p")
local line, column = tonumber(arg[2]) - 1, tonumber(arg[3]) - 1
local report = { file = file, line = line + 1, samples = { baseline = {}, current = {} }, warmup_ms = {} }
local function run()
  for iteration = 0, tonumber(arg[4]) or 3 do
    for _, name in ipairs(iteration % 2 == 1 and { "baseline", "current" } or { "current", "baseline" }) do
      local exe = name == "baseline" and vim.env.FLOWISTRY_BASELINE_EXE or vim.env.FLOWISTRY_BACKEND_EXE
      local done, err, result
      local start = uv.hrtime()
      backend.request({ root = vim.fs.root(file, "Cargo.toml"), env = {}, command = { assert(exe) } },
        { "file-focus", file, tostring(line), tostring(column) }, { timeout_ms = 300000, gzip = "gzip" },
        function(e, value) err, result, done = e, value, true end)
      assert(vim.wait(300000, function() return done end, 20), "timeout")
      assert(not err, err)
      local elapsed = (uv.hrtime() - start) / 1e6
      local analyzed = 0
      for _, body in ipairs(result.bodies) do
        if body.focus ~= vim.NIL then assert(body.focus.Ok, vim.inspect(body.focus)); analyzed = analyzed + 1 end
      end
      assert(analyzed == 1, "benchmark must analyze exactly one selected body")
      if iteration == 0 then
        report.warmup_ms[name] = elapsed
        print(("%s warmup: %.1f ms"):format(name, elapsed))
      else
        report.samples[name][#report.samples[name] + 1] = elapsed
        print(("%s #%d: %.1f ms"):format(name, iteration, elapsed))
      end
    end
  end
  report.median_ms = {}
  for name, samples in pairs(report.samples) do
    local sorted = vim.deepcopy(samples)
    table.sort(sorted)
    report.median_ms[name] = sorted[math.ceil(#sorted / 2)]
  end
  print(vim.json.encode(report))
  if vim.env.FLOWISTRY_BENCH_REPORT then vim.fn.writefile({ vim.json.encode(report) }, vim.env.FLOWISTRY_BENCH_REPORT) end
end
local ok, err = xpcall(run, debug.traceback)
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
vim.cmd("qa!")
