local M = {}
function M.check()
  vim.health.start("flowistry.nvim")
  if vim.fn.has("nvim-0.10") == 1 then vim.health.ok("Neovim >= 0.10")
  else vim.health.error("Neovim >= 0.10 is required") end
  for _, executable in ipairs({ "cargo", "rustc", "gzip" }) do
    if vim.fn.executable(executable) == 1 then vim.health.ok(executable .. " is available")
    else vim.health.warn(executable .. " is missing from PATH (unless a custom backend command supplies it)") end
  end
  vim.health.info("Default backend: Flowistry 0.5.44 at 693ceda925bd1d39d8de413ce239cfa6a87bb665; nightly-2026-05-01")
  vim.health.info("See :help flowistry-backend for installation and custom commands.")
end
return M
