-- Track a source anchor across edits, including formatters which replace whole
-- lines and therefore move extmarks to the end of otherwise unchanged text.
local M = {}
M.namespace = vim.api.nvim_create_namespace("flowistry.pin")

local function diff(before, after)
  return vim.diff(before, after, { result_type = "indices", algorithm = "histogram" })
end

local function token_range(line, column)
  local offset = 0
  while offset < #line do
    local match = vim.fn.matchstrpos(line, [[\k\+]], offset)
    if match[2] < 0 then break end
    if match[2] <= column and column < match[3] then return match[2], match[3] end
    offset = match[3]
  end
  return column, math.min(column + 1, #line)
end

-- Represent each source byte as one diff line, including embedded newlines and
-- UTF-8 bytes. This second diff is only needed for a changed block containing
-- the pin; ordinary line insertions need only the first, line-based diff.
local function bytes(text)
  return (text:gsub(".", function(char) return ("%02x\n"):format(char:byte()) end))
end

local function map_block(before, after, offset, first, last)
  local shift = 0
  for _, hunk in ipairs(diff(bytes(before), bytes(after))) do
    local start = hunk[1] - (hunk[2] > 0 and 1 or 0)
    local finish = start + hunk[2]
    -- Do not silently follow a different identifier if the pinned token was
    -- deleted/changed. Keep the old anchor so an undo can recover it.
    if (hunk[2] > 0 and start < last and finish > first)
      or (hunk[2] == 0 and first < start and start < last) then return nil end
    if finish <= first then shift = shift + hunk[4] - hunk[2] end
  end
  return offset + shift
end

local function relocate(pin, lines)
  local row, column = pin.pos[1], pin.pos[2]
  local shift = 0
  for _, hunk in ipairs(diff(table.concat(pin.lines, "\n") .. "\n", table.concat(lines, "\n") .. "\n")) do
    local start = hunk[1] - (hunk[2] > 0 and 1 or 0)
    if row < start then break end
    if row < start + hunk[2] then
      if hunk[4] == 0 then return nil end
      local old = table.concat(pin.lines, "\n", hunk[1], hunk[1] + hunk[2] - 1)
      local new = table.concat(lines, "\n", hunk[3], hunk[3] + hunk[4] - 1)
      local prefix = 0
      for index = hunk[1], row do prefix = prefix + #pin.lines[index] + 1 end
      local offset = map_block(old, new, prefix + column, prefix + pin.first, prefix + pin.last)
      if not offset then return nil end
      local head = new:sub(1, offset)
      local _, newlines = head:gsub("\n", "")
      local last_newline = head:match(".*()\n") or 0
      return { hunk[3] - 1 + newlines, offset - last_newline }
    end
    shift = shift + hunk[4] - hunk[2]
  end
  return { row + shift, column }
end

local function remember(buf, pin, lines, pos)
  pin.lines, pin.pos = lines, pos
  pin.tick = vim.api.nvim_buf_get_changedtick(buf)
  pin.first, pin.last = token_range(lines[pos[1] + 1] or "", pos[2])
  pin.id = vim.api.nvim_buf_set_extmark(buf, M.namespace, pos[1], pos[2], {
    id = pin.id, sign_text = "📌", sign_hl_group = "FlowistryPin", priority = 1000,
  })
end

function M.set(buf, pos, previous)
  local pin = { id = previous and previous.id }
  remember(buf, pin, vim.api.nvim_buf_get_lines(buf, 0, -1, false), pos)
  return pin
end

function M.position(buf, pin)
  local tick = vim.api.nvim_buf_get_changedtick(buf)
  if pin.tick == tick then return pin.pos end
  if pin.missing_tick == tick then return nil end
  local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  local pos = relocate(pin, lines)
  if pos then
    local line = lines[pos[1] + 1] or ""
    local first, last = token_range(line, pos[2])
    if line:sub(first + 1, last) ~= pin.lines[pin.pos[1] + 1]:sub(pin.first + 1, pin.last) then pos = nil end
  end
  if not pos then
    -- Keep the source anchor for undo, but never point at a different line.
    if pin.id then vim.api.nvim_buf_del_extmark(buf, M.namespace, pin.id); pin.id = nil end
    pin.missing_tick = tick
    return nil
  end
  remember(buf, pin, lines, pos)
  pin.missing_tick = nil
  return pos
end

function M.contains(buf, pin, pos)
  local anchor = M.position(buf, pin)
  return anchor ~= nil and anchor[1] == pos[1]
    and (pos[2] == anchor[2] or (pin.first <= pos[2] and pos[2] < pin.last))
end

function M.clear(buf)
  if vim.api.nvim_buf_is_valid(buf) then vim.api.nvim_buf_clear_namespace(buf, M.namespace, 0, -1) end
end

return M
