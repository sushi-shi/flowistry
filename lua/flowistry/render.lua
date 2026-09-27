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
  vim.api.nvim_set_hl(0, "FlowistryInfluence", { default = true, link = "CursorLine" })
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

function M.show(buf, focus, pos, priority, show_influence)
  M.clear(buf)
  local token = ranges.token(buf, pos)
  if not token then return nil end
  local place = ranges.smallest(focus.places, pos, function(item) return item.range end)
  if not place or #place.ranges == 0 then return nil end
  local slice = vim.list_extend(vim.deepcopy(place.slice), place.ranges)
  highlight(buf, ranges.complement(focus.containers, slice), "FlowistryDim", priority)
  if show_influence then
    highlight(buf, place.direct_influence, "FlowistryInfluence", priority + 1)
  end
  highlight(buf, { token }, "FlowistryFocus", priority + 2)
  return ranges.merge(slice)
end

return M
