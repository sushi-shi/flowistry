-- Session-only setup. The user's existing Neovim configuration stays loaded.
local flow = require("flowistry")
local backend_exe = assert(vim.env.FLOWISTRY_BACKEND_EXE, "Missing Flowistry backend")
flow.setup(vim.tbl_deep_extend("force", {
  command = { backend_exe },
  gzip = vim.env.FLOWISTRY_GZIP or "gzip",
  root = vim.env.FLOWISTRY_WORKSPACE,
  batch = true,
  progress = true,
  timeout_ms = 300000,
}, vim.g.flowistry_config or {}))
require("flowistry.statusline").setup()
for _, binding in ipairs({
  { "<leader>ft", "toggle", "Toggle Flowistry focus" },
  { "<leader>fm", "mark", "Pin Flowistry focus" },
  { "<leader>fu", "unmark", "Unpin Flowistry focus" },
}) do
  if vim.fn.maparg(binding[1], "n") == "" then
    vim.keymap.set("n", binding[1], flow[binding[2]], { desc = binding[3] })
  end
end
