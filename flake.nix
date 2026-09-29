{
  description = "Flowistry: ownership-aware information-flow analysis for Rust";

  inputs = {
    # Same pins as flowistry.nvim, so both evaluate to the same toolchain store
    # path and neither re-fetches it.
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
        in {
          inherit backend toolchain;
          default = backend;
        });

      devShells = eachSystem ({ pkgs, toolchain, compilerLibraries, ... }: {
        default = pkgs.mkShell {
          # mkShell's stdenv supplies `cc` for build scripts and linking.
          packages = [ toolchain pkgs.pkg-config pkgs.python3 pkgs.time ];
          SYSROOT = "${toolchain}";
          LD_LIBRARY_PATH = compilerLibraries;
        };

        # The default shell plus the native libraries that the git repositories in the
        # smoke-test corpus (scripts/smoke-corpus) need to build: niri and alacritty.
        smoke = pkgs.mkShell {
          packages = [ toolchain pkgs.pkg-config pkgs.python3 pkgs.time pkgs.git pkgs.cmake ];
          nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];
          buildInputs = with pkgs; [
            cairo dbus libGL libdisplay-info_0_3 libinput seatd libxkbcommon libgbm pango
            wayland systemd pipewire fontconfig freetype libxcb
          ];
          SYSROOT = "${toolchain}";
          LD_LIBRARY_PATH = compilerLibraries;
        };
      });
    };
}
