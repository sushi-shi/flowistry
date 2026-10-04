if vim.g.loaded_flowistry then return end
vim.g.loaded_flowistry = true

local commands = { "toggle", "enable", "disable", "mark", "unmark", "refresh", "types", "pre", "post", "both", "project", "start", "stop", "log" }
local aliases = { on = "enable", off = "disable", pin = "mark", unpin = "unmark" }
local actions = { "on", "off", "pin", "unpin", "toggle", "refresh", "types", "pre", "post", "both", "project", "start", "stop", "log" }
local function dispatch(args)
  if args.args == "" then
    vim.ui.select(actions, { prompt = "Flowistry action:", format_item = function(action)
      return ({ pre = "pre — what affects this value", post = "post — what this value affects",
        both = "both — causes and effects" })[action] or action
    end }, function(action)
      if action then dispatch({ args = action }) end
    end)
    return
  end
  local action = args.args
  action = aliases[action] or action
  if not vim.tbl_contains(commands, action) then
    vim.notify("Flowistry: unknown action " .. action, vim.log.levels.ERROR)
    return
  end
  require("flowistry")[action]()
end
for _, name in ipairs({ "Flow", "Flowistry" }) do
  local completions = name == "Flow" and actions or commands
  vim.api.nvim_create_user_command(name, dispatch, {
    nargs = "?",
    desc = "Focus on Rust code related to the cursor",
    complete = function(lead)
      return vim.tbl_filter(function(action) return action:sub(1, #lead) == lead end, completions)
    end,
  })
end

for _, action in ipairs({ "toggle", "mark", "unmark", "refresh", "pre", "post", "both" }) do
  vim.keymap.set("n", "<Plug>(Flowistry" .. action:sub(1, 1):upper() .. action:sub(2) .. ")", function()
    require("flowistry")[action]()
  end, { silent = true, desc = "Flowistry " .. action })
end
