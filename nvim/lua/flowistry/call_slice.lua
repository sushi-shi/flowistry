-- A pin's saved source slice can be rendered in any of its files. Keep filename
-- identities and source validation here rather than borrowing a buffer's cursor.
local M = {}
local ranges = require("flowistry.ranges")
local uv = vim.uv or vim.loop
local function canonical(path) return uv.fs_realpath(path) or vim.fs.normalize(path) end

function M.prepare(value, root)
  assert(value.schema == 1 and type(value.bodies) == "table" and vim.islist(value.bodies)
    and type(value.files) == "table", "Invalid pinned slice response")
  local files, by_path = {}, {}
  for id, source in pairs(value.files) do
    assert(type(source) == "table" and type(source.path) == "string" and type(source.text) == "string",
      "Missing pinned source identity")
    local path = source.path
    if not path:match("^/") and not path:match("^%a:[/\\]") then path = root .. "/" .. path end
    local lines = vim.split(source.text:gsub("\r\n", "\n"), "\n", { plain = true })
    if lines[#lines] == "" and #lines > 1 then table.remove(lines) end
    local file = { path = canonical(path), lines = lines }
    files[tostring(id)], by_path[file.path] = file, file
  end
  local function convert(buf)
    local convert_range = ranges.converter(buf, root)
    return function(range)
      assert(type(range) == "table", "Invalid pinned range")
      local file = assert(files[tostring(range.filename)], "Unknown pinned source file")
      return convert_range({ filename = file.path, start = range.start, ["end"] = range["end"] })
    end
  end
  local self = { files = by_path }
  local prepared = {}
  function self:buffer(buf)
    local name = canonical(vim.api.nvim_buf_get_name(buf))
    local file = by_path[name]
    if not file then return nil end
    local tick = vim.api.nvim_buf_get_changedtick(buf)
    local previous = prepared[buf]
    if previous and previous.tick == tick and previous.name == name then return previous.focus end
    local ok, disk = pcall(vim.fn.readfile, name)
    if not ok or not vim.deep_equal(disk, file.lines)
      or not vim.deep_equal(vim.api.nvim_buf_get_lines(buf, 0, -1, false), file.lines) then
      return nil, "Pinned source changed; save or reload it, then pin the value again"
    end
    local result = { containers = {}, comments = {}, pre_slice = {}, post_slice = {},
      maybe_pre_slice = {}, maybe_post_slice = {} }
    local bodies = {}
    local to_range = convert(buf)
    for _, body in ipairs(value.bodies) do
      local range = to_range(body.range)
      if range then
        bodies[#bodies + 1] = range
        for field, list in pairs(result) do
          vim.list_extend(list, ranges.convert_list(body[field], to_range))
        end
      end
    end
    result.bodies = bodies
    prepared[buf] = { name = name, tick = tick, focus = result }
    return result
  end
  return self
end

return M
