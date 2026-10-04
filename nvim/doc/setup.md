# Everyday Neovim setup

Flowistry does not require CoC, rust-analyzer, a completion plugin, or a language
server. It runs its own Rust analysis backend. You can use Neovim's built-in LSP,
CoC, another client, or no language server alongside it.

## Install

Try it from your Rust project's development environment:

```sh
nix run github:sushi-shi/flowistry/top#nvim -- src/main.rs
```

This launcher loads your existing Neovim configuration and supplies the paired
plugin and backend. For ordinary `nvim` to load Flowistry automatically, install
the flake's `packages.<system>.plugin` in your editor and call `setup()` as below.
See [NixOS and Home Manager installation](nix.md) for both approaches and updates.
The plugin and backend come from one locked revision; you do not install a
separate backend or point the plugin at another checkout.

## Configure ordinary Neovim

After installing the paired plugin, add this to your Lua configuration:

```lua
require("flowistry").setup({ progress = true })
require("flowistry.statusline").setup()
```

Progress and small error popups use native Neovim floating windows. If CoC is
loaded, Flowistry uses its notification UI instead. Errors show the actual
diagnostic, disappear after five seconds, and leave full details in `:Flow log`.
The popup includes that command. The log also reports failures of the selected
function when analysis of other functions succeeds. A missing `libclang` error
comes from a project's bindgen build script: use the project's development shell,
which must supply `LIBCLANG_PATH`, just as for a normal Cargo build.
Neither popup takes keyboard focus. The status indicator supports the ordinary
Neovim statusline and airline; neither airline nor CoC is required.
The `|` separator is added only when airline displays a nonempty CoC status,
giving `rust-analyzer | flowistry`. Otherwise the label is simply `flowistry`.

The launcher already calls setup. When using it, configure options with
`vim.g.flowistry_config = { ... }` instead of calling setup a second time.

Set `context_mode = "Recurse"` in `setup()` to distinguish the fields a local
method actually reads. The signature-based default conservatively includes the
whole receiver. Constructor field labels select their initializer value where
the compiler exposes it; unrelated fields remain dimmed. Callee analysis can
increase the first request's cost, and completed results are cached.

## Two-letter shortcuts

These optional normal-mode mappings match the `s`-prefix setup. They deliberately
disable Vim's standalone `s` substitute command. `ss`, `sp`, and `su` apply only
to Rust buffers and leave existing mappings for those keys alone. `fp` and `fu`
keep their normal find-character behavior.

Put this after your other keybindings, with either installation method:

```lua
vim.keymap.set("n", "s", "<Nop>", { silent = true, desc = "Shortcut prefix" })

local function flowistry_keys(buf)
  for _, binding in ipairs({
    { "ss", "toggle", "Toggle Flowistry focus" },
    { "sp", "pin", "Pin Flowistry focus" },
    { "su", "unpin", "Unpin Flowistry focus" },
  }) do
    if vim.fn.maparg(binding[1], "n") == "" then
      vim.keymap.set("n", binding[1], "<Cmd>Flow " .. binding[2] .. "<CR>",
        { buffer = buf, silent = true, desc = binding[3] })
    end
  end
end

vim.api.nvim_create_autocmd("FileType", {
  group = vim.api.nvim_create_augroup("UserFlowistryKeys", { clear = true }),
  pattern = "rust",
  callback = function(args) flowistry_keys(args.buf) end,
})
```

Restart Neovim, open a saved Rust file in a Cargo project, and put the cursor on
a variable. Flowistry enables automatically. `sp` pins the focus and adds a red
📌 above ordinary letter-mark signs; `su` removes the pin and reveals the mark
underneath. `ss` turns analysis off or on. The plugin itself installs no mappings
on these keys. Use `:verbose nmap sp` to locate an existing mapping if it wins.

## Rust completion with manual checks (optional CoC example)

This section configures a separate language-server client, not Flowistry. Skip
it if you do not use CoC; configure your chosen client independently.

For `coc-rust-analyzer`, make `rust-analyzer`, `cargo`, and `rustc` available in
Neovim's environment. The Flowistry plugin's private compiler does not put Cargo
or rustc on the general PATH. [The Nix guide](nix.md#rust-in-your-general-environment)
shows how to install them for terminals and language servers.

Merge these entries into your existing `:CocConfig` JSON object:

```json
{
  "rust-analyzer.server.path": "rust-analyzer",
  "rust-analyzer.checkOnSave": false,
  "rust-analyzer.cargo.buildScripts.enable": true,
  "rust-analyzer.procMacro.enable": true
}
```

Restart Neovim after changing its environment. These settings disable
check-on-save while allowing the builds needed to understand generated code and
procedural macros. Disabling the last two settings also disables those builds,
but rust-analyzer can then report `macro-error` and miss macro-generated items.
These settings belong to rust-analyzer; they do not configure Flowistry.

Flowistry has its own compiler work: first analysis and changed inputs can
compile dependencies. The CoC settings do not disable that. Use `:Flow off` for
the current buffer or `:Flow stop` for all Flowistry analysis in the session.
Project background analysis is opt-in and disabled by default.

## Build when you choose

From your Cargo project directory:

```sh
cargo check
cargo build
cargo test
```

Inside Neovim, `:!cargo build` runs a manual build from the editor's current
directory (`:pwd`). These commands remain available when language-server builds
are disabled. Manual Cargo builds still execute the build scripts and compile
the macros they need.

On NixOS, enter the project's development shell before building or launching
Neovim, so its linker and native dependencies are available:

```sh
nix develop
nvim src/main.rs
```

Use that project's documented environment if it has no flake. For developing
Flowistry itself, its default `nix develop` shell supplies the matching compiler,
linker, and compiler libraries. Installing the general Rust toolchain alone does
not supply every project's native dependencies.

Flowistry analyzes the code compiled by the selected Cargo target and features.
A file behind `#[cfg(target_os = "ios")]`, for example, is absent from a Linux
build. Analyze it in the project's supported iOS build environment; enabling
Flowistry does not enable excluded modules. An empty compiler response is
reported as missing analysis, with the command and diagnostics in `:Flow log`.

## Check the setup

- `:checkhealth flowistry` checks Flowistry's prerequisites.
- `:Flow log` shows the complete latest analysis error.
- `:echo executable('cargo')` and `:echo executable('rustc')` should each return
  `1` if you installed Rust for your language server and manual builds.
- For CoC users, `:CocInfo` shows the separate language-server status.
