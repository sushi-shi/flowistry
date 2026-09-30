local M = {}
-- A conditional group preserves the literal gap between adjacent flag
-- expressions and hides the gap entirely when Flowistry is disabled.
local expression = "%( %{v:lua.require('flowistry.statusline').text()}%)"

function M.text()
  if vim.bo.buftype ~= "" or (vim.bo.filetype ~= "rust" and not vim.api.nvim_buf_get_name(0):match("%.rs$")) then
    return ""
  end
  local status = require("flowistry").status()
  if status == "off" then return "" end
  local suffix = {
    active = "", idle = "", pinned = " (pinned)", loading = " (analyzing)",
    ["waiting for save"] = " (save needed)", ["outside function"] = " (outside function)",
    ["no place"] = " (select a variable)", editing = " (editing)",
    ["pinned target unavailable"] = " (pinned target unavailable)",
    ["analysis unavailable"] = " (unavailable)", error = " (error)",
  }
  local separator = ""
  if vim.g.loaded_airline == 1 then
    local ok, coc_status = pcall(vim.fn["airline#extensions#coc#get_status"])
    if ok and vim.trim(coc_status) ~= "" then separator = "| " end
  end
  return separator .. "flowistry" .. (suffix[status] or "")
    .. (require("flowistry").is_stale() and " [saved analysis]" or "")
end

local function append(value, part)
  -- Vimscript string() doubles quotes inside %! expressions.
  if value:find("flowistry.statusline", 1, true) then return value end
  if value:sub(1, 2) == "%!" then
    return "%!(" .. value:sub(3) .. ") . " .. vim.fn.string(expression)
  end
  return value .. (part or expression)
end

function M.attach()
  if vim.g.loaded_airline == 1 then
    -- Airline creates its defaults during initialization, including bufferline
    -- and CoC's rust-analyzer status. Append after those existing parts.
    if vim.g.airline_section_c then
      local coc = vim.fn["airline#parts#get"]("coc_status")
      vim.fn["airline#parts#define"]("flowistry", { raw = expression, accent = coc.accent or "bold" })
      local part = vim.fn["airline#section#create"]({ "flowistry" })
      local section = append(vim.g.airline_section_c, part)
      if section ~= vim.g.airline_section_c then
        vim.g.airline_section_c = section
        vim.cmd("AirlineRefresh")
      end
    end
  else
    local current = vim.wo.statusline
    vim.wo.statusline = append(current ~= "" and current or "%f %m%r%=%l:%c")
  end
end

function M.setup()
  local group = vim.api.nvim_create_augroup("FlowistrySession", { clear = true })
  vim.api.nvim_create_autocmd("User", { group = group, pattern = "AirlineAfterInit", callback = M.attach })
  vim.api.nvim_create_autocmd({ "BufWinEnter", "FileType", "VimEnter" }, { group = group, callback = M.attach })
  M.attach()
end

return M
