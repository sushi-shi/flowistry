{
  description = "Flowistry: ownership-aware information-flow analysis for Rust";

  inputs = {
    # One compiler and dependency lock for the backend and all editor packages.
    nixpkgs.url = "github:NixOS/nixpkgs/1559d3daa3ecc813a650b79375ea61b6741b8746";
    fenix.url = "github:nix-community/fenix/5f7e7d793cb2553410f857554de86f277ebe2f71";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, fenix }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f rec {
        inherit system;
        pkgs = import nixpkgs { inherit system; };
        # Matches rust-toolchain.toml.
        toolchain = (fenix.packages.${system}.toolchainOf {
          channel = "nightly";
          date = "2026-05-01";
          sha256 = "ea96e87fce61b2182006a819cf8d6ac74cc7562e4bc6d60aba4107ffcba58d52";
        }).withComponents [ "cargo" "rustc" "rust-std" "rustc-dev" "rust-src" "llvm-tools-preview" ];
        compilerLibraries = "${toolchain}/lib:${toolchain}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/lib";
      });
    in {
      packages = eachSystem ({ pkgs, toolchain, compilerLibraries, ... }:
        let
          rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };
          binPath = pkgs.lib.makeBinPath [ toolchain pkgs.stdenv.cc pkgs.pkg-config ];
          backend = rustPlatform.buildRustPackage {
            pname = "flowistry-backend";
            version = "0.5.44-${self.shortRev or self.dirtyShortRev or "dev"}";
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "flowistry_ide" ];
            doCheck = false;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            LD_LIBRARY_PATH = compilerLibraries;
            postFixup = ''
              for executable in cargo-flowistry flowistry-driver; do
                wrapProgram "$out/bin/$executable" \
                  --prefix PATH : "${binPath}:$out/bin" \
                  --prefix LD_LIBRARY_PATH : "${compilerLibraries}" \
                  --set SYSROOT "${toolchain}"
              done
              makeWrapper "${toolchain}/bin/cargo" "$out/bin/flowistry-backend" \
                --add-flags flowistry \
                --prefix PATH : "$out/bin:${binPath}" \
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
            version = self.shortRev or self.dirtyShortRev or "dev";
            src = ./nvim;
            # Loading this plugin directly also selects this checkout's backend.
            # No second repository input, revision pin or ambient backend lookup.
            postInstall = ''
              cat > "$out/lua/flowistry/packaged.lua" <<'LUA'
              return {
                command = { "${backend}/bin/flowistry-backend" },
                gzip = "${pkgs.gzip}/bin/gzip",
                batch = true,
              }
              LUA
            '';
          };
          nvim = pkgs.writeShellScriptBin "flowistry-nvim" ''
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
          inherit backend toolchain plugin nvim;
          default = backend;
        });

      apps = eachSystem ({ system, ... }: {
        nvim = {
          type = "app";
          meta.description = "Neovim with Flowistry from the same checkout";
          program = "${self.packages.${system}.nvim}/bin/flowistry-nvim";
        };
      });

      checks = eachSystem ({ pkgs, system, ... }:
        let
          packages = self.packages.${system};
          test = name: script: inputs: pkgs.runCommand name {
            nativeBuildInputs = [ pkgs.neovim pkgs.gzip ] ++ inputs;
          } ''
            export XDG_STATE_HOME="$TMPDIR/state"
            export XDG_CACHE_HOME="$TMPDIR/cache"
            export CARGO_HOME="$TMPDIR/cargo"
            cp -r ${./nvim} source
            chmod -R u+w source
            cd source
            ${script}
            touch "$out"
          '';
          backendTest = name: script: test name ''
            export FLOWISTRY_BACKEND_EXE=${packages.backend}/bin/flowistry-backend
            ${script}
          '' [ packages.backend pkgs.python3 ];
        in {
          nvim-frontend = test "flowistry-nvim-frontend" "make test" [ pkgs.gnumake ];
          nvim-source-selection = backendTest "flowistry-nvim-source-selection" ''
            nvim --headless -u NONE -i NONE -l tests/source-selection.lua
            nvim --headless -u NONE -i NONE -l tests/precision.lua
          '';
          nvim-summaries = backendTest "flowistry-nvim-summaries" ''
            nvim --headless -u NONE -i NONE -l tests/summaries.lua
          '';
          nvim-cache = backendTest "flowistry-nvim-cache" ''
            nvim --headless -u NONE -i NONE -l tests/cache.lua
            FLOWISTRY_TEST_RUSTFMT=${pkgs.rustfmt}/bin/rustfmt nvim --headless -u NONE -i NONE -l tests/layout_live.lua
            python3 ${./scripts/test-focus-cache.py} --backend "$FLOWISTRY_BACKEND_EXE"
            python3 ${./scripts/test-fast-cache.py} --backend "$FLOWISTRY_BACKEND_EXE"
          '';
          nvim-package = test "flowistry-nvim-package" ''
            export FLOWISTRY_EXPECTED_BACKEND=${packages.backend}/bin/flowistry-backend
            export FLOWISTRY_TEST_PLUGIN=${packages.plugin}
            nvim --headless -u NONE -i NONE -l tests/package.lua
            FLOWISTRY_TEST_SESSION=1 ${packages.nvim}/bin/flowistry-nvim \
              --headless -u NONE -i NONE -c 'luafile tests/package.lua'
          '' [ packages.nvim packages.plugin ];
        });

      devShells = eachSystem ({ pkgs, toolchain, compilerLibraries, ... }: {
        default = pkgs.mkShell {
          # mkShell's stdenv supplies `cc` for build scripts and linking.
          packages = [ toolchain pkgs.pkg-config pkgs.python3 pkgs.time ];
          SYSROOT = "${toolchain}";
          LD_LIBRARY_PATH = compilerLibraries;
        };

        nvim = pkgs.mkShell {
          packages = [ pkgs.neovim pkgs.gzip pkgs.gnumake self.packages.${pkgs.stdenv.hostPlatform.system}.backend ];
          FLOWISTRY_BACKEND_EXE = "${self.packages.${pkgs.stdenv.hostPlatform.system}.backend}/bin/flowistry-backend";
          FLOWISTRY_BATCH = "1";
        };

        # The default shell plus the native libraries that the git repositories in the
        # smoke-test corpus (scripts/smoke-corpus) need to build: niri, alacritty and
        # bosminer (libusb).
        smoke = pkgs.mkShell {
          packages = [ toolchain pkgs.pkg-config pkgs.python3 pkgs.time pkgs.git pkgs.cmake ];
          nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];
          buildInputs = with pkgs; [
            cairo dbus libGL libdisplay-info_0_3 libinput seatd libxkbcommon libgbm pango
            wayland systemd pipewire fontconfig freetype libxcb libusb1
          ];
          SYSROOT = "${toolchain}";
          LD_LIBRARY_PATH = compilerLibraries;
        };
      });
    };
}
