# NixOS and Home Manager

Enable Nix's `nix-command` and `flakes` experimental features. The lock file pins
nixpkgs, Fenix, the Rust nightly (including rustc-dev), and upstream Flowistry;
backend changes are shipped as patches. No ambient rustup installation is needed.

Try the launcher from your Cargo project's development shell:

```sh
nix run github:sushi-shi/flowistry.nvim -- src/main.rs
```

The launcher uses your existing `nvim` and configuration if available, otherwise
the pinned Neovim package. It adds the plugin and sets up the matching backend,
gzip, progress UI, and status indicator. Native project libraries still need to
come from the project's shell.

For a private checkout, use SSH with a GitHub-authorized key:

```sh
nix run 'git+ssh://git@github.com/sushi-shi/flowistry.nvim' -- src/main.rs
```

Use the same SSH URL for the input below if the repository is private. A local
clone also works with `nix run . -- /path/to/src/main.rs`.

## Install the launcher on NixOS

Add the input to your system flake:

```nix
inputs.flowistry.url = "github:sushi-shi/flowistry.nvim";
```

Pass `inputs` through `specialArgs = { inherit inputs; };` to your NixOS modules,
then install the launcher:

```nix
{ inputs, pkgs, ... }:
{
  environment.systemPackages = [
    inputs.flowistry.packages.${pkgs.stdenv.hostPlatform.system}.default
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
        command = { "${flowistry.backend}/bin/flowistry-backend" },
        gzip = "${pkgs.gzip}/bin/gzip",
        batch = true,
        progress = true,
        context_mode = "Recurse", -- omit for signature-based analysis
      })
      require("flowistry.statusline").setup()
    '';
  };
}
```

These store-path references retain the backend and gzip in the Neovim closure;
no additional PATH or dynamic-library configuration is required. The bare
`plugin` package contains only the frontend and needs backend setup as above.
Use either this setup or the launcher for a session to avoid configuring twice.
Keep this input's nixpkgs pin when reproducibility matters: the compiler/backend
build environment has been tested with it.

## Packages and checks

| Output | Purpose |
| --- | --- |
| `packages.<system>.default` | `flowistry-nvim` launcher |
| `packages.<system>.plugin` | Neovim runtime plugin |
| `packages.<system>.backend` | Shared backend and compiler-aware wrapper |
| `packages.<system>.toolchain` | Matching Rust toolchain |
| `devShells.<system>.default` | Frontend development and real-backend tests |
| `checks.<system>.frontend` | Headless frontend regressions |
| `checks.<system>.summaries` | Real compiler and highlight regressions |

Run `nix flake check` to build and test for your host system. Linux outputs exist
for x86_64 and aarch64; x86_64 is build-tested here. Run `nix build .#plugin
.#backend .#default` to build the installable packages. No macOS outputs are
currently provided.
