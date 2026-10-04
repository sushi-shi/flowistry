local M = {}
M.namespace = vim.api.nvim_create_namespace("flowistry")
local ranges = require("flowistry.ranges")

function M.highlights()
  vim.api.nvim_set_hl(0, "FlowistryDim", {
    default = true,
    fg = vim.o.background == "light" and "#8899ad" or "#62758a",
    ctermfg = vim.o.background == "light" and 103 or 60,
    italic = false, bold = false,
  })
  vim.api.nvim_set_hl(0, "FlowistryFocus", { default = true, link = "Visual" })
  vim.api.nvim_set_hl(0, "FlowistryPin", { default = true, fg = "#ff5555", ctermfg = 196 })
  vim.api.nvim_set_hl(0, "FlowistryInfluence", { default = true, link = "CursorLine" })
  -- Code that matters only if two shared handles (e.g. Rc<RefCell<T>> clones)
  -- point to the same object: kept readable, but tinted apart from the slice.
  vim.api.nvim_set_hl(0, "FlowistryMaybe", {
    default = true,
    fg = vim.o.background == "light" and "#9a6f1e" or "#c49a55",
    ctermfg = vim.o.background == "light" and 136 or 179,
    italic = true,
  })
end

function M.clear(buf)
  if vim.api.nvim_buf_is_valid(buf) then vim.api.nvim_buf_clear_namespace(buf, M.namespace, 0, -1) end
end

local function highlight(buf, items, group, priority)
  for _, range in ipairs(ranges.merge(items)) do
    vim.api.nvim_buf_set_extmark(buf, M.namespace, range.start[1], range.start[2], {
      end_row = range.finish[1], end_col = range.finish[2], hl_group = group,
      priority = priority, hl_mode = "replace", strict = true,
    })
  end
end

function M.selection(focus, pos, parameter_types)
  for _, comment in ipairs(focus.comments or {}) do
    if ranges.contains(comment, pos) then return nil end
  end
  if parameter_types ~= false then
    local alias = ranges.smallest(focus.parameter_aliases or {}, pos, function(item) return item.range end)
    if alias then return alias.target.start end
  end
  return pos
end

function M.show(buf, focus, pos, priority, show_influence, show_maybe, parameter_types, direction)
  M.clear(buf)
  pos = M.selection(focus, pos, parameter_types)
  if not pos then return nil end
  local token = ranges.token(buf, pos)
  if not token then return nil end
  local place = ranges.smallest(focus.places, pos, function(item) return item.range end)
  if not place or #place.ranges == 0 then return nil end
  direction = direction or "both"
  local selected = direction == "both" and place.slice or place[direction .. "_slice"]
  if not selected then return nil, "Directional focus requires an updated Flowistry backend. Use :Flow both or update the backend." end
  local slice = vim.list_extend(vim.deepcopy(selected), place.ranges)
  local maybe = show_maybe ~= false and (direction == "both" and place.maybe_slice or place["maybe_" .. direction .. "_slice"]) or {}
  maybe = maybe or {}
  local shown = vim.list_extend(vim.deepcopy(slice), maybe)
  -- Comment tokens keep their syntax colors even when a broad MIR source span
  -- includes them. Subtract from every decoration, including optional modes.
  local function without_comments(items) return ranges.complement(items, focus.comments or {}) end
  highlight(buf, without_comments(ranges.complement(focus.containers, shown)), "FlowistryDim", priority)
  highlight(buf, without_comments(maybe), "FlowistryMaybe", priority)
  if show_influence then
    local visible = ranges.complement(place.direct_influence, ranges.complement(focus.containers, shown))
    highlight(buf, without_comments(visible), "FlowistryInfluence", priority + 1)
  end
  highlight(buf, { token }, "FlowistryFocus", priority + 2)
  return without_comments(slice)
end

function M.pinned(buf, focus, pos, priority, show_maybe, direction)
  M.clear(buf)
  local function selected(prefix)
    if direction == "both" then
      return vim.list_extend(vim.deepcopy(focus[prefix .. "pre_slice"]), focus[prefix .. "post_slice"])
    end
    return vim.deepcopy(focus[prefix .. direction .. "_slice"])
  end
  local slice = selected("")
  local token = pos and ranges.token(buf, pos)
  if token then slice[#slice + 1] = token end
  local maybe = show_maybe ~= false and ranges.complement(selected("maybe_"), slice) or {}
  local shown = vim.list_extend(vim.deepcopy(slice), maybe)
  local function without_comments(items) return ranges.complement(items, focus.comments) end
  highlight(buf, without_comments(ranges.complement(focus.containers, shown)), "FlowistryDim", priority)
  highlight(buf, without_comments(maybe), "FlowistryMaybe", priority)
  if token then highlight(buf, { token }, "FlowistryFocus", priority + 2) end
  return without_comments(slice)
end

return M
