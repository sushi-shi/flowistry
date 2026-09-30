-- Watch hints only invalidate editor state; they never establish cache validity.
local M = {}
local uv = vim.uv or vim.loop
local function canonical(path) return uv.fs_realpath(path) or vim.fs.normalize(path) end
local function within(path, root) return path == root or path:sub(1, #root + 1) == root .. "/" end
function M.scope(filename, target)
  target = target or {}
  return table.concat({ target.package_id or target.package or "", target.target_kind or "",
    target.target_name or "", canonical(filename) }, "\0")
end

function M.new()
  local self = { workspaces = {} }
  function self:observe(root, value, scope)
    if not root then return end
    root = canonical(root)
    local watch = self.workspaces[root] or { roots = {}, files = {}, unknown = {}, bytes = 0, count = 0 }
    self.workspaces[root] = watch
    if watch.overflow then return end
    scope = scope or root
    local function unknown()
      watch.unknown[scope] = true
      if vim.tbl_count(watch.unknown) > 128 then watch.overflow = true; watch.unknown = {} end
    end
    -- A successful observation repairs this target/file's unknown hint, while
    -- retaining other scopes and all previously discovered dependency paths.
    if type(value) ~= "table" or value == vim.NIL or value.schema ~= 1
      or not vim.islist(value.roots) or not vim.islist(value.files) then unknown(); return end
    for _, field in ipairs({ "roots", "files" }) do
      for _, path in ipairs(value[field]) do
        if type(path) ~= "string" or path:sub(1, 1) ~= "/" then unknown(); return end
        path = canonical(path)
        if not watch[field][path] then
          watch.bytes, watch.count = watch.bytes + #path, watch.count + 1
          if watch.bytes > 1024 * 1024 or watch.count > 8192 then
            watch.roots, watch.files, watch.overflow = {}, {}, true
            return
          end
          watch[field][path] = true
        end
      end
    end
    watch.unknown[scope] = nil
  end
  function self:matches(root, path)
    if not root or path == "" then return false end
    root, path = canonical(root), canonical(path)
    if within(path, root) then return true end
    local watch = self.workspaces[root]
    if not watch then return false end
    if watch.overflow or next(watch.unknown) or watch.files[path] then return true end
    for directory in pairs(watch.roots) do if within(path, directory) then return true end end
    return false
  end
  function self:status(root)
    local watch = root and self.workspaces[canonical(root)]
    if not watch then return { reason = "workspace-only-before-discovery" } end
    return { reason = watch.overflow and "watch-budget-exceeded"
      or (next(watch.unknown) and "missing-or-invalid-input-metadata" or "compiler-snapshot-inputs"),
      roots = vim.tbl_count(watch.roots), files = vim.tbl_count(watch.files),
      unknown_scopes = vim.tbl_count(watch.unknown), path_bytes = watch.bytes }
  end
  return self
end
return M
