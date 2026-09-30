# NixOS and Home Manager

Enable Nix's `nix-command` and `flakes` experimental features. The lock file pins
nixpkgs, Fenix and the Rust nightly (including rustc-dev). The root flake builds
the plugin and backend from this same repository checkout, so no second source
revision can drift. No ambient rustup installation is needed.

Try the launcher from your Cargo project's development shell:

```sh
nix run github:sushi-shi/flowistry#nvim -- src/main.rs
```

The launcher uses your existing `nvim` and configuration if available, otherwise
the pinned Neovim package. It adds the plugin and sets up the matching backend,
gzip, progress UI, and status indicator. Native project libraries still need to
come from the project's shell.

For a private checkout, use SSH with a GitHub-authorized key:

```sh
nix run 'git+ssh://git@github.com/sushi-shi/flowistry#nvim' -- src/main.rs
```

Use the same SSH URL for the input below if the repository is private. A local
clone also works with `nix run .#nvim -- /path/to/src/main.rs`.

## Install the launcher on NixOS

Add the input to your system flake:

```nix
inputs.flowistry.url = "github:sushi-shi/flowistry/top";
```

Pass `inputs` through `specialArgs = { inherit inputs; };` to your NixOS modules,
then install the launcher:

```nix
{ inputs, pkgs, ... }:
{
  environment.systemPackages = [
    inputs.flowistry.packages.${pkgs.stdenv.hostPlatform.system}.nvim
  ];
}
```

Launch `flowistry-nvim src/main.rs`. Set options in your usual Neovim config:

```lua
vim.g.flowistry_config = { context_mode = "Recurse" }
```

## Add to Home Manager's Neovim

Pass the same `inputs` via Home Manager's `extraSpecialArgs`, then add this module:

```nix
{ inputs, pkgs, ... }:
let
  flowistry = inputs.flowistry.packages.${pkgs.stdenv.hostPlatform.system};
in {
  programs.neovim = {
    enable = true;
    plugins = [ flowistry.plugin ];
    extraLuaConfig = ''
      require("flowistry").setup({
        progress = true,
        context_mode = "Recurse", -- omit for signature-based analysis
      })
      require("flowistry.statusline").setup()
    '';
  };
}
```

The plugin automatically binds its matching backend and gzip store paths. Both
are retained in the Neovim closure; no backend pin, PATH or dynamic-library
configuration is required.
Use either this setup or the launcher for a session to avoid configuring twice.
Keep this input's nixpkgs pin when reproducibility matters: the compiler/backend
build environment has been tested with it.

If you configure Neovim through `pkgs.neovim.override` instead of
`programs.neovim`, add the same `flowistry.plugin` package to
`configure.packages.<yourPackageSet>.start`. Add the Lua setup to your existing
configuration (or a `lua << EOF` block in `configure.customRC`). Keep your other
plugins and configuration. The [everyday setup guide](setup.md) provides the
optional `ss`/`sp`/`su` mappings and explains the popups. CoC is optional.

## Rust in your general environment

The paired backend carries its own compiler, but this does not make `cargo` and
`rustc` available to terminals or language servers. To use the matching toolchain
there too, add it to your Home Manager module:

```nix
{ inputs, pkgs, ... }:
{
  home.packages = [
    inputs.flowistry.packages.${pkgs.stdenv.hostPlatform.system}.toolchain
    pkgs.rust-analyzer # optional language server, independent of Flowistry
  ];
}
```

For a NixOS module, use `environment.systemPackages` instead. Alternatively,
install just the compiler into your user profile:

```sh
nix profile add github:sushi-shi/flowistry/top#toolchain
```

The general compiler does not replace a project's development shell or native
libraries. See [manual builds and optional CoC settings](setup.md) for keeping
builds under your control. Flowistry does not install or configure an LSP client.

## Update the paired installation

The `top` branch follows the review-stack tip; your configuration's `flake.lock`
keeps using its recorded revision until you update it. From your configuration
flake's directory, run:

```sh
nix flake update flowistry
```

Then build and activate your Neovim configuration through your usual NixOS or
Home Manager workflow, and restart Neovim. Both plugin and backend update together.
If your configuration exposes the configured editor as a `neovim` package, you
can install only that package without activating other system changes:

```sh
nix build .#neovim
nix profile add .#neovim # first profile installation only
nix profile upgrade neovim # subsequent updates of that profile entry
```

Here `.#neovim` is an output of your own configuration flake; Flowistry's `.#nvim`
output is the launcher. Check `nix profile list` for the installed entry's name
if it differs. Keep the Flowistry input's own nixpkgs/compiler pins instead of
adding an `inputs.nixpkgs.follows` override.

## Packages and checks

| Output | Purpose |
| --- | --- |
| `packages.<system>.nvim` | `flowistry-nvim` launcher |
| `packages.<system>.plugin` | Neovim runtime plugin bound to the matching backend |
| `packages.<system>.backend` / `default` | Shared backend and compiler-aware wrapper |
| `packages.<system>.toolchain` | Matching Rust toolchain |
| `devShells.<system>.nvim` | Frontend development and real-backend tests |
| `checks.<system>.nvim-frontend` | Headless frontend regressions |
| `checks.<system>.nvim-summaries` | Real compiler and highlight regressions |
| `checks.<system>.nvim-cache` | Editor reuse and cross-process cache invalidation |

Run `nix flake check` from the Flowistry repository root to build and test for your host system. Linux outputs exist
for x86_64 and aarch64; x86_64 is build-tested here. Run `nix build .#plugin
.#backend .#default` to build the installable packages. No macOS outputs are
currently provided.
