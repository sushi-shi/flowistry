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

function M.show(buf, focus, pos, priority, show_influence, show_maybe, parameter_types)
  M.clear(buf)
  pos = M.selection(focus, pos, parameter_types)
  if not pos then return nil end
  local token = ranges.token(buf, pos)
  if not token then return nil end
  local place = ranges.smallest(focus.places, pos, function(item) return item.range end)
  if not place or #place.ranges == 0 then return nil end
  local slice = vim.list_extend(vim.deepcopy(place.slice), place.ranges)
  local maybe = show_maybe ~= false and place.maybe_slice or {}
  local shown = vim.list_extend(vim.deepcopy(slice), maybe)
  -- Comment tokens keep their syntax colors even when a broad MIR source span
  -- includes them. Subtract from every decoration, including optional modes.
  local function without_comments(items) return ranges.complement(items, focus.comments or {}) end
  highlight(buf, without_comments(ranges.complement(focus.containers, shown)), "FlowistryDim", priority)
  highlight(buf, without_comments(maybe), "FlowistryMaybe", priority)
  if show_influence then
    highlight(buf, without_comments(place.direct_influence), "FlowistryInfluence", priority + 1)
  end
  highlight(buf, { token }, "FlowistryFocus", priority + 2)
  return without_comments(slice)
end

return M
