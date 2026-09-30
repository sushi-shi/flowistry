local M = {}

local function before(a, b)
  return a[1] < b[1] or (a[1] == b[1] and a[2] < b[2])
end
M.before = before

function M.contains(range, pos)
  return not before(pos, range.start) and before(pos, range.finish)
end

-- Restrict cursor selection to a word. A compiler span can cover an entire
-- call chain, including dots and whitespace which aren't useful focus targets.
function M.token(buf, pos)
  local line = vim.api.nvim_buf_get_lines(buf, pos[1], pos[1] + 1, false)[1] or ""
  local offset = 0
  while offset < #line do
    local match = vim.fn.matchstrpos(line, [[\k\+]], offset)
    if match[2] < 0 or match[2] > pos[2] then return nil end
    if pos[2] < match[3] then
      return { start = { pos[1], match[2] }, finish = { pos[1], match[3] } }
    end
    offset = match[3]
  end
end

local function canonical(path)
  return (vim.uv or vim.loop).fs_realpath(path) or vim.fs.normalize(path)
end

-- rustc_utils uses zero-based Unicode scalar columns, not bytes or UTF-16.
local function byte_column(line, column)
  assert(type(column) == "number" and column >= 0 and column % 1 == 0, "Invalid column")
  local offset = vim.fn.byteidxcomp(line, column)
  assert(offset >= 0, "Analysis column is outside the saved buffer")
  return offset
end

function M.position(buf, cursor)
  local line = vim.api.nvim_buf_get_lines(buf, cursor[1] - 1, cursor[1], false)[1] or ""
  -- strchars counts combining characters separately, as Rust char_indices does.
  return { cursor[1] - 1, vim.fn.strchars(line:sub(1, cursor[2])) }
end

function M.converter(buf, root, source_id)
  local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  local filename = canonical(vim.api.nvim_buf_get_name(buf))
  local function point(pos)
    assert(type(pos) == "table" and type(pos.line) == "number", "Invalid position")
    assert(pos.line >= 0 and pos.line % 1 == 0 and lines[pos.line + 1], "Analysis line is outside the saved buffer")
    return { pos.line, byte_column(lines[pos.line + 1], pos.column) }
  end
  return function(range)
    assert(type(range) == "table", "Invalid source range")
    if type(range.filename) == "number" then
      -- Flowistry 0.5.44 serializes an opaque, per-response FilenameIndex.
      assert(type(source_id) == "number", "Missing source file identity")
      if range.filename ~= source_id then return nil end
    else
      assert(type(range.filename) == "string", "Invalid source filename")
      local path = range.filename
      if not path:match("^/") and not path:match("^%a:[/\\]") then
        path = root .. "/" .. path
      end
      if canonical(path) ~= filename then
        return nil -- Macro expansions may refer to other files.
      end
    end
    local result = { start = point(range.start), finish = point(range["end"]) }
    assert(not before(result.finish, result.start), "Reversed source range")
    return result
  end
end

function M.convert_list(items, convert)
  assert(type(items) == "table" and vim.islist(items), "Expected a list of source ranges")
  local result = {}
  for _, item in ipairs(items) do
    local range = convert(item)
    if range then
      result[#result + 1] = range
    end
  end
  return result
end

function M.merge(ranges)
  local sorted = vim.deepcopy(ranges)
  table.sort(sorted, function(a, b) return before(a.start, b.start) end)
  local result = {}
  for _, range in ipairs(sorted) do
    if before(range.start, range.finish) then
      local last = result[#result]
      if last and not before(last.finish, range.start) then
        if before(last.finish, range.finish) then last.finish = range.finish end
      else
        result[#result + 1] = range
      end
    end
  end
  return result
end

-- Complement of the union of the slice, clipped to each function container.
function M.complement(containers, slice)
  local result = {}
  for _, container in ipairs(M.merge(containers)) do
    local cursor = container.start
    for _, range in ipairs(M.merge(slice)) do
      if before(range.start, container.finish) and before(cursor, range.finish) then
        if before(cursor, range.start) then
          result[#result + 1] = { start = cursor, finish = range.start }
        end
        cursor = before(range.finish, container.finish) and range.finish or container.finish
      end
    end
    if before(cursor, container.finish) then
      result[#result + 1] = { start = cursor, finish = container.finish }
    end
  end
  return result
end

function M.smallest(items, pos, get_range)
  local best
  for _, item in ipairs(items) do
    local range = get_range(item)
    if M.contains(range, pos) then
      local previous = best and get_range(best)
      if not previous or before(previous.start, range.start)
        or (vim.deep_equal(previous.start, range.start) and before(range.finish, previous.finish)) then
        best = item
      end
    end
  end
  return best
end

return M
