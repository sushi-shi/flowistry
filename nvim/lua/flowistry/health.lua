local M = {}
function M.check()
  vim.health.start("flowistry.nvim")
  if vim.fn.has("nvim-0.10") == 1 then vim.health.ok("Neovim >= 0.10")
  else vim.health.error("Neovim >= 0.10 is required") end
  for _, executable in ipairs({ "cargo", "rustc", "gzip" }) do
    if vim.fn.executable(executable) == 1 then vim.health.ok(executable .. " is available")
    else vim.health.warn(executable .. " is missing from PATH (unless a custom backend command supplies it)") end
  end
  local ok, packaged = pcall(require, "flowistry.packaged")
  if ok then vim.health.ok("Paired backend: " .. packaged.command[1])
  else vim.health.info("Source checkout: use the matching backend from nix develop .#nvim") end
  vim.health.info("See :help flowistry-backend for installation and custom commands.")
end
return M
