# flowistry.nvim

Focus on the Rust code related to the variable under your cursor. Unrelated code
inside the current function is dimmed; relevant code keeps its syntax colors.

This is a Neovim frontend for [Flowistry](https://github.com/willcrichton/flowistry).
The **analysis backend stays editor-independent**: the existing Rust compiler
plugin computes ownership-aware information flow. Lua handles editor events,
process transport, caching, and decorations. A future Zed frontend can use that
same executable and protocol; see [the backend contract](doc/backend.md).

## Install the paired editor and backend

The plugin lives in `nvim/` in the Flowistry repository. The root flake builds
both the Rust backend and this plugin from the same source tree; there is no
separate backend revision to update. The installed plugin automatically selects
that build's backend, even when loaded without the launcher.

From your Rust project's development environment:

```sh
nix run github:sushi-shi/flowistry#nvim -- /path/to/project/src/main.rs
```

For a local Flowistry checkout, use `nix run /path/to/flowistry#nvim -- FILE`, or
`/path/to/flowistry/tools/flowistry FILE`. The launcher loads your existing Neovim
configuration and supplies the plugin, backend, compiler libraries and gzip.
Project-specific native libraries still come from your project's shell.
Neovim 0.10 or newer and a saved Rust file are required; the launcher supplies a
pinned Neovim if none is on PATH. It does not modify the project's toolchain.

Root flake outputs:

- `.#nvim`: configured `flowistry-nvim` launcher.
- `.#plugin`: Neovim plugin with its matching backend bound automatically.
- `.#backend` (also the default package): editor-independent Rust backend.
- `.#toolchain`: matching Rust compiler.

See [NixOS and Home Manager setup](doc/nix.md) for installation. Linux packages
exist for x86_64 and aarch64. [Callee analysis](doc/summaries.md) explains the
optional `context_mode = "Recurse"` mode; signature-based analysis is the default.

## Load the plugin directly

Install the root flake's `packages.<system>.plugin` into your Neovim configuration
and call `require("flowistry").setup()`. Backend selection is automatic. You can
still override `command` explicitly for backend development.

For frontend development, enter `nix develop /path/to/flowistry#nvim`, add
`/path/to/flowistry/nvim` to `runtimepath`, and configure:

```lua
require("flowistry").setup({
  command = { assert(vim.env.FLOWISTRY_BACKEND_EXE) },
  batch = true,
})
```

Re-enter the development shell after backend changes to rebuild the paired
backend. The old `sushi-shi/flowistry.nvim` repository is historical; use this
repository for code, issues and PRs.

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

A red 📌 in the sign column marks the pinned line. It takes priority over ordinary
letter-mark signs without deleting them; unpinning reveals them again. The marker
follows edits and formatting, disappears if the pinned token is removed, and
returns if undo restores it. Customize its color with `FlowistryPin`.

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
affected results. First-time analysis can still take seconds. Optional project
background warming is described below. See [persistent caching](doc/cache.md) for scope,
controls and limits.
Unsaved Rust buffers or manifests in the workspace suspend new analysis; the
plugin never writes buffers for you. Editing preserves the last successful
highlights and moves pins with the text. The status marks this as saved analysis.
Saving reanalyzes automatically, retaining the pin. Failed compilation preserves
the previous display until a successful save or explicit disable.
An analysis failure shows a small, non-focusing error popup in the same area as
analysis progress. It contains the compiler diagnostic, closes after five seconds,
and keeps the full details in `:Flow log`.
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
  show_maybe = true,             -- tint code that matters only if shared handles alias
  parameter_types = true,        -- an argument's type selects its binding (map: &LevelMap)
  progress = false,              -- analysis popups; enabled by the Nix launcher
  project = { enabled = false }, -- opt-in bounded workspace background analysis
})
```

For the `./tools/flowistry` / Nix launcher, set overrides in your Neovim config:

```lua
vim.g.flowistry_config = { auto_enable = false }
```

With a backend that supplies source-selection metadata, comments retain their
syntax colors and cannot be selected as dataflow targets. `parameter_types`
also makes the entire argument type, including `&` and generic arguments, behave
like its binding. Set it to `false` to keep ordinary cursor selection. Destructured
arguments are left unchanged because their type does not identify one binding.
Use `:Flow types` to toggle type selection during a session without clearing caches.
Older backends remain supported and keep their existing behavior.

With `auto_enable=false`, use `:Flow on` when you want analysis. The launcher
merges these overrides with its packaged backend settings.

### Project background analysis

On Linux with a working systemd user session, use `:Flow project` to toggle
workspace warming, or configure `project = { enabled = true }`. It requires the
shared cache and this repository's matching backend. The backend discovers Cargo
workspace targets; the editor maintains one queue per workspace and visits its
library/binary targets, prioritizing the active file and other enabled buffers.
Targets requiring opt-in features are skipped by default. Exact target source
files select that target; shared modules prefer their package's library. Override
the selection when a module is shared by targets with different configurations:

```lua
project = {
  enabled = true,
  targets = { { package = "my-package", target_kind = "lib", target_name = "my_library" } },
  -- targets may also be a function(workspace_root) returning such a list.
  features = "optional-feature", -- applies to foreground and background together
  idle_ms = 300,
  max_workspaces = 1,            -- active background workspaces globally
  memory_mib = 6144,             -- per compiler/Cargo worker scope
  timeout_seconds = 600,        -- per worker; an oversized body cannot stop the queue
  max_body_bytes = 8 * 1024 * 1024,
  max_results_bytes = 16 * 1024 * 1024,
}
```

Foreground requests cancel the workspace's current background worker and wait
for cleanup before starting. Identical foreground requests share one operation.
Warming resumes after foreground work settles. Saves cancel old generations;
unsaved Rust/manifest buffers pause their workspace. This increment conservatively
restarts the inventory after saves; dependency-selective save planning is separate
work. Failed bodies remain visible in project status and do not block other bodies.

The editor decodes only results for open enabled buffers. Background decode and
retention budgets count uncompressed JSON bytes; actual Lua heap use also depends
on representation and is not equal to those byte counts. Old background objects
are evicted while their validated backend entries remain reusable. Larger results
stay in the backend store and can be analyzed on demand. Foreground analysis keeps
its existing behavior and the 600-line batch guard. The project-wide stream never
claims that results from different saves form one current project snapshot.

`:Flow stop` disables all buffers and background jobs and suspends automatic
enabling; `:Flow start` restores automatic enabling. Ordinary `:Flow off` remains
per-buffer. `require("flowistry").project_status()` exposes body/target progress,
failed outcomes and retained background bytes; `:Flow log` includes its diagnostic.
Background mode remains opt-in pending large-project and latency acceptance gates.

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
vim.api.nvim_set_hl(0, "FlowistryMaybe", { fg = "#c49a55", italic = true })
```

Dimmed code uses a dedicated muted blue-gray foreground, distinct from comments,
with a 256-color terminal fallback. Focus links to `Visual`. Direct-influence
backgrounds link to `CursorLine` and are disabled by default to avoid resembling
extra selections. Related code keeps its ordinary syntax colors. Independent plain
constructor fields are dimmed when following an input forward; backward selection
retains the constructor inputs. Comments keep their syntax colors.
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

Run the following commands from the `nvim/` directory:

```sh
nvim --headless -u NONE -i NONE -l tests/run.lua
```

From the repository root, `nix flake check` runs the frontend and real-compiler callee-summary
checks in isolated build environments. `nix develop .#nvim` provides Neovim, Make,
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
lines, changes indentation, and verifies the focus through eleven actual saves.
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
