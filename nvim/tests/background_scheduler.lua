local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
local backend = require("flowistry.backend")
local manager_module = require("flowistry.background")
local root = vim.fn.tempname() .. " scheduler"
local valid, dirty, streams, deliveries, decoded, evictions = {}, {}, {}, {}, {}, {}
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local function await(test, message) check(vim.wait(2000, test, 5), message) end
local function fixture(name)
  local path = root .. "/" .. name
  vim.fn.mkdir(path .. "/src", "p")
  vim.fn.writefile({ "fn main() {}" }, path .. "/src/main.rs")
  local buf = vim.fn.bufadd(path .. "/src/main.rs"); vim.fn.bufload(buf)
  local target = { package = name, target_kind = "bin", target_name = name,
    manifest_path = path .. "/Cargo.toml", src_path = path .. "/src/main.rs", supported = true }
  local state = { buf = buf, context = { root = path, command = { "fixture" }, targets = { target } } }
  valid[buf] = state
  return state, path
end
backend.stream = function(context, args, config, event, done)
  local op = { context = context, args = args, event = event, done = done }
  function op:cancel() self.cancelled = true end
  streams[#streams + 1] = op
  return op
end
backend.decode = function(output, config, callback)
  local op = { output = output, callback = callback }
  function op:cancel() self.cancelled = true end
  decoded[#decoded + 1] = op
  return op
end
local config = { project = { enabled = true, idle_ms = 10, memory_mib = 384, timeout_seconds = 5, max_results_bytes = 15, max_workspaces = 2 } }
local m = manager_module.new(config, {
  valid = function(state) return valid[state.buf] == state end,
  dirty = function(path) return dirty[path] end,
  result = function(state, value) deliveries[#deliveries + 1] = { state, value } end,
  evict = function(_, identity) evictions[#evictions + 1] = identity end,
})
local a, path_a = fixture("a")
local b, path_b = fixture("b")
vim.api.nvim_set_current_buf(a.buf)
local function body(state, identity)
  return { event = "body", status = "current", current = true, output = "fixture", body = { identity = identity,
    range = { filename = vim.api.nvim_buf_get_name(state.buf), start = { line = 0, column = 0 }, ["end"] = { line = 0, column = 12 } } } }
end
local ok, err = xpcall(function()
  m:attach(a); m:attach(a)
  await(function() return #streams == 1 end, "duplicate attachment starts one workspace coordinator")
  check(vim.tbl_contains(streams[1].args, "--cursor-file"), "active buffer is prioritized")
  m:attach(b)
  await(function() return #streams == 2 end, "another workspace has an independent queue")
  local starts, answers, finish = 0, {}, nil
  local function launch(done)
    starts = starts + 1; finish = done
    return { cancel = function() answers.child_cancelled = true end }
  end
  local first = m:foreground(a.context, "same-request", launch, function(_, value) answers.first = value end)
  local second = m:foreground(a.context, "same-request", launch, function(_, value) answers.second = value end)
  check(streams[1].cancelled and not streams[2].cancelled, "foreground preempts only its workspace")
  check(starts == 0, "foreground waits for coordinator cleanup completion")
  streams[1].done(nil, { cancelled = true, code = 130 })
  check(starts == 1, "identical foreground requests coalesce")
  first:cancel()
  check(not answers.child_cancelled, "one canceled subscriber does not kill shared work")
  finish(nil, "result")
  check(answers.first == nil and answers.second == "result", "only the live subscriber receives the result")
  await(function() return #streams == 3 end, "background resumes after foreground result delivery")

  streams[3].event(body(a, "one"))
  check(#decoded == 1, "current open-buffer result is decoded")
  dirty[path_a] = true
  m:invalidate(path_a)
  check(streams[3].cancelled and decoded[1].cancelled, "edit cancels both worker and decoding")
  decoded[1].callback(nil, { stale = true })
  check(#deliveries == 0, "late decode cannot cross an edit generation")
  streams[3].done(nil, { cancelled = true, code = 130 })
  vim.wait(50, function() return false end, 5)
  check(#streams == 3, "dirty workspace does not restart")
  dirty[path_a] = nil
  m:invalidate(path_a, path_a .. "/src/other.rs")
  m:invalidate(path_a, path_a .. "/src/main.rs")
  m:invalidate(path_a, path_a .. "/src/other.rs")
  await(function() return #streams == 4 end, "save starts a new generation")
  local saved = {}
  for i, arg in ipairs(streams[4].args) do
    if arg == "--saved-file" then saved[#saved + 1] = streams[4].args[i + 1] end
  end
  check(vim.deep_equal(saved, { path_a .. "/src/other.rs", path_a .. "/src/main.rs" }),
    "rapid saves coalesce with the latest file first and no duplicates")
  streams[4].event(body(a, "two"))
  decoded[2].callback(nil, { fresh = true }, 10)
  check(#deliveries == 1 and deliveries[1][2].fresh, "current generation is delivered")
  check(m:status(path_a).retained_result_bytes == 10, "background retention accounts for decoded bytes")
  streams[4].event(body(a, "three"))
  decoded[3].callback(nil, { another = true }, 10)
  check(evictions[1] == "two" and m:status(path_a).retained_result_bytes == 10, "old background results are evicted within the shared budget")
  streams[4].event(body(a, "too-large"))
  decoded[4].callback(nil, { large = true }, 20)
  check(#deliveries == 2 and m:status(path_a).retained_result_bytes == 10, "oversized background results remain backend-only")
  local missing = body(a, "unopened")
  missing.body.range.filename = path_a .. "/src/unopened.rs"
  streams[4].event(missing)
  check(#decoded == 4, "unopened buffers are warmed only in the backend store")
  streams[4].event(body(a, "uncurrent"))
  valid[a.buf] = nil
  decoded[5].callback(nil, { detached = true })
  check(#deliveries == 2, "disabled buffers reject pending results")
  valid[a.buf] = a

  m:detach(a)
  check(streams[4].cancelled, "last buffer detachment cancels its workspace")
  streams[4].done(nil, { cancelled = true, code = 130 })
  vim.wait(50, function() return false end, 5)
  check(#streams == 4, "detached workspace stays idle")
  m:attach(a)
  await(function() return #streams == 5 end, "reattachment resumes valid work")
  local canceled = m:foreground(a.context, "abandoned", launch, function() error("canceled request delivered") end)
  canceled:cancel()
  streams[5].done(nil, { cancelled = true, code = 130 })
  check(starts == 1, "canceled queued foreground request never starts")
  await(function() return #streams == 6 end, "abandoned request releases its foreground slot")
  m:close()
  check(streams[2].cancelled and streams[6].cancelled, "global close stops every workspace")
  streams[2].done(nil, { cancelled = true, code = 130 })
  streams[6].done(nil, { cancelled = true, code = 130 })
  vim.wait(50, function() return false end, 5)
  check(#streams == 6, "late completion cannot revive a closed scheduler")

  local another = manager_module.new(config, {
    valid = function(state) return valid[state.buf] == state end,
    dirty = function() return false end, result = function() end,
  })
  another:attach(b)
  await(function() return #streams == 7 end, "fresh scheduler starts")
  local failed = body(b, "failed-body")
  failed.current, failed.status = false, "worker_error"
  streams[7].event(failed)
  streams[7].event({ event = "finished", status = "partial" })
  streams[7].done(nil, { code = 1, cancelled = false })
  vim.wait(50, function() return false end, 5)
  check(another:status(path_b).status == "partial" and another:status(path_b).failed == 1,
    "partial project failure cannot become a successful completion")
  another:rediscover(path_b)
  vim.wait(50, function() return false end, 5)
  check(#streams == 7, "manifest invalidation waits for fresh target metadata")
  b.context.targets[1].target_name = "replacement"
  another:attach(b)
  await(function() return #streams == 8 end, "fresh target inventory resumes scheduling")
  check(vim.tbl_contains(streams[8].args, "replacement"), "scheduler uses the new target identity")
  another:close()
  streams[8].done(nil, { code = 130, cancelled = true })

  config.project.max_workspaces = 1
  local bounded = manager_module.new(config, {
    valid = function(state) return valid[state.buf] == state end,
    dirty = function() return false end, result = function() end,
  })
  bounded:attach(a)
  await(function() return #streams == 9 end, "one global background slot starts")
  bounded:attach(b)
  vim.wait(50, function() return false end, 5)
  check(#streams == 9 and bounded:status(path_b).status == "queued", "additional workspace stays within the global worker budget")
  streams[9].event(body(a, "decoding"))
  streams[9].event({ event = "finished", status = "complete" })
  streams[9].done(nil, { code = 0, cancelled = false })
  vim.wait(50, function() return false end, 5)
  check(#streams == 9, "pending decode retains the global workspace slot")
  decoded[#decoded].callback(nil, {}, 10)
  await(function() return #streams == 10 end, "completed workspace releases its slot to the next workspace")
  check(streams[10].context.root == path_b, "waiting workspace makes progress")
  bounded:close()
  streams[10].done(nil, { code = 130, cancelled = true })
end, debug.traceback)
m:close()
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Flowistry scheduler: %d assertions passed"):format(passed))
vim.cmd("qa!")
