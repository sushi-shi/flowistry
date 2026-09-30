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
inputs.flowistry.url = "github:sushi-shi/flowistry";
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
