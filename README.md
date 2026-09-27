# flowistry.nvim

Focus on the Rust code related to the variable under your cursor. Unrelated code
inside the current function is dimmed; relevant code keeps its syntax colors.

This is a Neovim frontend for [Flowistry](https://github.com/willcrichton/flowistry).
The **analysis backend stays editor-independent**: the existing Rust compiler
plugin computes ownership-aware information flow. Lua handles editor events,
process transport, caching, and decorations. A future Zed frontend can use that
same executable and protocol; see [the backend contract](doc/backend.md).

## Requirements

- Neovim 0.10 or newer.
- `gzip` on PATH, for decoding the backend's compressed JSON.
- Flowistry and its matching Rust toolchain, installed separately.
- A saved Rust file in a Cargo project that builds with that toolchain.

## Backend installation

The package targets Flowistry fork revision
[`9528045feec9f48abf4383f9b43bdcf7e0c6c341`](https://github.com/sushi-shi/flowistry/tree/9528045feec9f48abf4383f9b43bdcf7e0c6c341),
which identifies itself as **0.5.44** and pins **nightly-2026-05-01**. The compiler
API and wire format are version-sensitive. Install that revision, rather than
assuming an arbitrary published version or latest nightly is compatible:

```sh
rustup toolchain install nightly-2026-05-01 \
  --component rust-src --component rustc-dev --component llvm-tools-preview
cargo +nightly-2026-05-01 install --locked \
  --git https://github.com/sushi-shi/flowistry \
  --rev 9528045feec9f48abf4383f9b43bdcf7e0c6c341 flowistry_ide
```

Make sure Cargo's bin directory is on Neovim's PATH. The frontend discovers the
compiler sysroot and dynamic library path before launching the backend. It does
not install tools or change your project's toolchain file.

On NixOS, the flake supplies the backend with its matching compiler and libraries:

```sh
nix run github:sushi-shi/flowistry.nvim -- /path/to/project/src/main.rs
```

This loads your existing Neovim configuration and adds Flowistry for the session.
The packages are separate: `.#backend` is the editor-independent executable,
`.#plugin` is the Neovim plugin, and the default package is the configured launcher.
For example, `nix build .#backend` exposes `result/bin/flowistry-backend` for use
by either an editor adapter or a command-line client. Inputs and the compiler
manifest are pinned. Build the source revision, not the older prebuilt release
that happens to carry the same upstream version number.

See [NixOS and Home Manager setup](doc/nix.md) to install the launcher or add the
plugin to your existing Neovim configuration. Linux packages are exposed for
`x86_64-linux` and `aarch64-linux`; the latter is evaluated but not build-tested.
Private repositories require authenticated Git access; the guide includes an
SSH URL. The first backend build may take several minutes.

The Nix backend also includes the fork's cached callee summaries. Opt in with
`context_mode = "Recurse"`; the default remains signature-based analysis.
See [callee analysis](doc/summaries.md) for scope, caching, and limitations.

Projects with native libraries should launch from their development environment.
For the local `stalker-mobile` checkout, `./tools/flowistry` handles that setup and
opens `crates/stalker-engine/src/gameplay.rs`. `FLOWISTRY_WORKSPACE` optionally sets the full
Cargo workspace root for the session launcher.

## Load the plugin

With lazy.nvim:

```lua
{
  "sushi-shi/flowistry.nvim",
  ft = "rust",
  cmd = { "Flow", "Flowistry" },
  opts = {},
  keys = {
    { "<leader>ft", "<Cmd>Flowistry toggle<CR>", desc = "Toggle Flowistry" },
    { "<leader>fm", "<Cmd>Flowistry mark<CR>", desc = "Pin Flowistry focus" },
    { "<leader>fu", "<Cmd>Flowistry unmark<CR>", desc = "Unpin Flowistry focus" },
  },
}
```

Or add this directory to `runtimepath`, then call `require("flowistry").setup()`.
No mappings are installed on ordinary keys. `<Plug>(FlowistryToggle)`,
`<Plug>(FlowistryMark)`, `<Plug>(FlowistryUnmark)`, and `<Plug>(FlowistryRefresh)`
are available.

## Use

1. Open a Rust file and place the cursor on a variable.
2. Flowistry enables automatically in saved Rust buffers belonging to a Cargo
   project. Use `:Flow off` to disable it for a buffer, and `:Flow on` to restore it.
3. Move between variables to explore their dependencies.
4. Run `:Flow pin` to pin the focus while reading other code. Repeat it on the
   pinned variable to unpin; use it on another variable to move the pin.
   `:Flow unpin` resumes cursor tracking from anywhere. `:Flow off` disables it.

| Command | Action |
| --- | --- |
| `:Flow on` / `off` | Enable or disable for the current buffer |
| `:Flow pin` / `unpin` | Pin the cursor position or resume following it |
| `:Flow` / `:Flowistry` | Show an action menu |
| `:Flow toggle` / `refresh` / `log` | Toggle, retry analysis, or show errors |
| `:Flowistry toggle` | Toggle focus mode |
| `:Flowistry enable` / `disable` | Enable or disable for the current buffer |
| `:Flowistry mark` / `unmark` | Pin or release the current position |
| `:Flowistry refresh` | Force fresh analysis, bypassing cached results |
| `:Flowistry log` | Show the current buffer's latest error; `q` closes the window |
| `:checkhealth flowistry` | Check basic prerequisites |

Analysis runs asynchronously. The Nix launcher shows status in the existing
statusline, beside the buffer list and language-server status when using airline:
`rust-analyzer | flowistry`, using the same text style. Disabled buffers hide the
Flowistry label; pinned buffers show `flowistry (pinned)`. Analysis opens a progress
popup with the current step and elapsed time, using CoC's animated notification
when available. It closes when the request finishes or is cancelled. Save reminders
and errors also appear in the statusline.

The packaged backend combines function discovery and analysis into one compiler
run. Files up to 600 lines are analyzed together, so entering another function
does not launch the compiler again. Larger files analyze the selected function
on demand. First-time analysis can still take seconds, especially in large
crates; the elapsed timer is activity feedback, not a percentage estimate.
Function results are cached in memory for the editor session, so moving within an
analyzed function does not run Cargo again. Nested functions and closures use the
smallest enclosing body. Unchanged saves, edit-and-undo, and off/on retain memory
results without a compiler request. Edits while disabled still invalidate them.
Off clears the pin, and enabling resumes cursor tracking.

The shared backend also stores successful results on disk. When recorded inputs
are unchanged, it replays a prepared response before starting Cargo or rustc;
warm requests for the measured large game function took about 200 ms. After
source or build inputs change, compiler validation runs, then the function cache
can reuse unchanged analysis. Blank lines and formatting relocate highlights;
changes to callees, types, constants, macros or build settings invalidate
affected results. First-time analysis can still take seconds. There is no idle
background cache warming yet. See [persistent caching](doc/cache.md) for scope,
controls and limits.
Unsaved Rust buffers or manifests in the workspace suspend new analysis; the
plugin never writes buffers for you. Editing preserves the last successful
highlights and moves pins with the text. The status marks this as saved analysis.
Saving reanalyzes automatically, retaining the pin. Failed compilation preserves
the previous display until a successful save or explicit disable.
Pins survive formatters that replace entire buffer lines, including rust.vim's
rustfmt-on-save. Source differences remap the pinned token after line or column
changes. If the target cannot be recovered, the status says `pinned target
unavailable`; undo the change, pin another variable, or run `:Flow unpin`.
Opening another file or switching buffers preserves existing pins and analysis
caches. New Rust buffers enable automatically; an explicit `:Flow off` stays off
when returning to that buffer or saving it.

The first analysis may take time to compile dependencies. Run `:Flowistry refresh`
after external dependency changes or backend configuration changes. All windows
showing the same buffer share its focus; the most recently active cursor controls
it. Cursor focus currently supports normal mode, not visual selections or
multiple selections.

## Configuration

```lua
require("flowistry").setup({
  auto_enable = true,            -- enable saved Rust buffers in Cargo projects
  toolchain = "nightly-2026-05-01", -- false uses the ambient compiler
  context_mode = nil,            -- default SigOnly; "Recurse" enables callee analysis
  cache = true,                 -- persistent backend results; memory cache stays enabled
  cache_dir = nil,              -- shared XDG cache by default
  command = nil,                 -- e.g. { "/path/to/flowistry-wrapper" }
  root = nil,                    -- explicit Cargo workspace root for a wrapper
  batch = false,                 -- true for the fork backend; Nix launcher sets it
  batch_max_lines = 600,          -- analyze larger files one function at a time
  env = {},                      -- extra environment, merged with the process env
  gzip = "gzip",
  debounce_ms = 120,
  timeout_ms = 180000,            -- per subprocess, including initial compilation
  priority = 200,                -- above normal syntax/semantic highlights
  show_influence = false,        -- optional extra direct-influence backgrounds
  progress = false,              -- analysis popups; enabled by the Nix launcher
})
```

For the `./tools/flowistry` / Nix launcher, set overrides in your Neovim config:

```lua
vim.g.flowistry_config = { auto_enable = false }
```

With `auto_enable=false`, use `:Flow on` when you want analysis. The launcher
merges these overrides with its packaged backend settings.

`command`, when set, is the full backend command prefix. The plugin appends
`spans FILE` or `focus FILE LINE COLUMN`; it does not invoke a shell. Custom commands
skip compiler and workspace discovery and run at `root`, or the nearest Cargo
manifest when unset. The wrapper must handle toolchain selection and environment
setup. Set `root` to the workspace root to include sibling crates when checking
for unsaved project inputs.

Customize these highlight groups through your colorscheme:

```lua
vim.api.nvim_set_hl(0, "FlowistryDim", { fg = "#606470" })
vim.api.nvim_set_hl(0, "FlowistryFocus", { bg = "#394457" })
vim.api.nvim_set_hl(0, "FlowistryInfluence", { bg = "#252b35" })
```

Dimmed code uses a dedicated muted blue-gray foreground, distinct from comments,
with a 256-color terminal fallback. Focus links to `Visual`. Direct-influence
backgrounds link to `CursorLine` and are disabled by default to avoid resembling
extra selections. Related code keeps its ordinary syntax colors; a struct may
stay bright as a whole when one of its fields depends on the selected variable.
Punctuation and whitespace do not select an enclosing expression. On a variable
or method name, the selection background covers only the word under the cursor;
the dependency analysis can still include a larger expression.
The packaged backend refines ordinary function calls: independent simple
arguments are dimmed when following a value forward, while selecting a call's
result keeps all contributing inputs visible. Complex expressions, macros and
method calls remain conservative.
Neovim uses a foreground color for dimming rather than VS Code's text opacity.
`require("flowistry").status()` returns the current state;
`require("flowistry").indicator()` returns a readable status with elapsed time.

## Development and validation

```sh
nvim --headless -u NONE -i NONE -l tests/run.lua
```

With Nix, `nix flake check` runs the frontend and real-compiler callee-summary
checks in isolated build environments. `nix develop` provides Neovim, Make,
gzip, and the packaged backend; run `make test-summaries` there to verify field
precision, nested calls, both request protocols, and invalidation after a save.
`make test-cache` checks real compiler cache reuse, moved pins, off/on, refresh,
and edits while disabled. The flake also runs the backend's cross-process cache
invalidation suite.

The headless suite exercises real Neovim extmarks and subprocess transport with a
synthetic backend fixture. It covers Unicode, range unions, nested bodies, cached
cursor movement, pins, edits/saves, cancellation, malformed output, and errors.
It requires Neovim and gzip, but no Rust installation. These tests verify the
frontend and wire handling; they do not validate Rust's analysis results.

With the matching backend installed, run the live smoke test:

```sh
nvim --headless -u NONE -i NONE -l tests/live.lua
```

The live test uses a temporary crate and verifies mutable-reference dependencies,
pins moving with edits, saved-analysis preservation, compiler errors and recovery.
Set `FLOWISTRY_BACKEND_EXE` to the packaged backend and `FLOWISTRY_BATCH=1` to
exercise combined analysis. `tests/precision.lua` checks call arguments, backward
dependencies, arrays, branches, references, Unicode and nested closures against
the real compiler. `tests/ui.lua` uses an installed airline/CoC configuration to
check text styles, separators, split windows and progress-window cleanup:

```sh
nvim --headless -n -i NONE --cmd 'let g:coc_start_at_startup=0' -c 'luafile tests/ui.lua'
```

`make test-save` uses the configured rust.vim formatter and the real backend
(`rustfmt` on PATH, `g:rustfmt_autosave=1`, and `FLOWISTRY_BACKEND_EXE` required).
It pins `dy` in a nested closure at line 5000 of a temporary crate, inserts blank
lines, changes indentation, and verifies the focus through ten actual saves.
The user's project files are never edited by this test.

`tests/performance.lua FILE ROW COLUMN` alternates baseline/current backend runs,
printing every sample and median; set `FLOWISTRY_BASELINE_EXE` and
`FLOWISTRY_BACKEND_EXE`. It measures compiler requests, not cached cursor moves.

For an interactive demo, open `examples/demo/src/main.rs` and focus `names` or
`scores`. The two independent data flows should become easy to distinguish.

## Analysis limits

Flowistry computes a conservative approximation of information flow for the
selected function. By default, call effects come from signatures; the optional
`Recurse` mode inspects supported local callees through cached field summaries.
Highlighted code may be relevant. Upstream documents limitations around interior mutability, closures,
async bodies, and mapping compiler IR back to source. This is a reading aid, not a
proof that dimmed code can be deleted or ignored in a correctness/security review.

The Rust backend remains a separate fork dependency. No Zed adapter or LSP server is
implemented here. The Lua frontend is MIT licensed; see [LICENSE](LICENSE) for
the upstream attribution.
