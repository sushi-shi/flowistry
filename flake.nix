{
  description = "Flowistry's shared Rust backend and Neovim frontend";

  inputs = {
    # Share the stalker-mobile development environment's package revision.
    nixpkgs.url = "github:NixOS/nixpkgs/1559d3daa3ecc813a650b79375ea61b6741b8746";
    fenix.url = "github:nix-community/fenix/5f7e7d793cb2553410f857554de86f277ebe2f71";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
    flowistry-src = {
      url = "github:willcrichton/flowistry/693ceda925bd1d39d8de413ce239cfa6a87bb665";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, fenix, flowistry-src }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      packages = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
          toolchain = (fenix.packages.${system}.toolchainOf {
            channel = "nightly";
            date = "2026-05-01";
            sha256 = "ea96e87fce61b2182006a819cf8d6ac74cc7562e4bc6d60aba4107ffcba58d52";
          }).withComponents [ "cargo" "rustc" "rust-std" "rustc-dev" "rust-src" "llvm-tools-preview" ];
          rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };
          compilerLibraries = "${toolchain}/lib:${toolchain}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/lib";
          backend = rustPlatform.buildRustPackage {
            pname = "flowistry-backend";
            version = "0.5.44-693ceda";
            src = flowistry-src;
            patches = [ ./patches/file-focus.patch ./patches/precise-focus.patch ];
            cargoLock.lockFile = "${flowistry-src}/Cargo.lock";
            cargoBuildFlags = [ "-p" "flowistry_ide" ];
            doCheck = false;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            LD_LIBRARY_PATH = compilerLibraries;
            postFixup = ''
              for executable in cargo-flowistry flowistry-driver; do
                wrapProgram "$out/bin/$executable" \
                  --prefix PATH : "${pkgs.lib.makeBinPath [ toolchain pkgs.stdenv.cc pkgs.pkg-config ]}:$out/bin" \
                  --prefix LD_LIBRARY_PATH : "${compilerLibraries}" \
                  --set SYSROOT "${toolchain}"
              done
              makeWrapper "${toolchain}/bin/cargo" "$out/bin/flowistry-backend" \
                --add-flags flowistry \
                --prefix PATH : "$out/bin:${pkgs.lib.makeBinPath [ toolchain pkgs.stdenv.cc pkgs.pkg-config ]}" \
                --prefix LD_LIBRARY_PATH : "${compilerLibraries}" \
                --set SYSROOT "${toolchain}"
            '';
            meta = {
              description = "Editor-independent ownership-aware Rust information-flow analysis";
              mainProgram = "flowistry-backend";
              license = pkgs.lib.licenses.mit;
            };
          };
          plugin = pkgs.vimUtils.buildVimPlugin {
            pname = "flowistry.nvim";
            version = "0.1.0";
            src = pkgs.lib.cleanSource ./.;
          };
          editor = pkgs.writeShellScriptBin "flowistry-nvim" ''
            set -euo pipefail
            editor=$(command -v nvim || true)
            if [ -z "$editor" ]; then editor=${pkgs.neovim}/bin/nvim; fi
            export FLOWISTRY_BACKEND_EXE=${backend}/bin/flowistry-backend
            export FLOWISTRY_GZIP=${pkgs.gzip}/bin/gzip
            exec "$editor" \
              --cmd 'set runtimepath^=${plugin}' \
              -c 'luafile ${plugin}/scripts/session.lua' "$@"
          '';
        in {
          inherit backend plugin toolchain;
          default = editor;
        });
      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/flowistry-nvim";
        };
      });
    };
}
