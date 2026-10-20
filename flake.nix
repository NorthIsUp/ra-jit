{
  description = "ra-jit dev shell: pinned nightly rustc/cargo + rust-src (rust-analyzer's sysroot), Python 3.12+";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    # x86_64-darwin is unsupported by current nixpkgs, so it is not listed.
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        # ra-jit needs the sysroot sources (rust-src) to resolve `core`.
        # Pinned nightly (cargo --unit-graph needs -Z unstable-options). The graph generator and every
        # per-unit rustc derivation use this one toolchain. 2026-07-15 is the last nightly before
        # the 1.99 branch, i.e. the closest to the stable 1.99.0 this project was written on.
        nightlyDate = "2026-07-15";
        rust = pkgs.rust-bin.nightly.${nightlyDate}.default.override {
          extensions = [ "rust-src" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rust;
          rustc = rust;
        };
        # eframe/winit dlopen these at run time.
        guiLibs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (with pkgs; [
          libGL
          libxkbcommon
          wayland
          libx11
          libxcursor
          libxi
          libxrandr
          libxcb
        ]);
        ra-jit = rustPlatform.buildRustPackage {
          pname = "ra-jit";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config pkgs.makeWrapper ];
          buildInputs = guiLibs;
          doCheck = false;
          # ra-jit asks `rustc --print sysroot` for core's sources, so the wrapped PATH carries
          # the pinned toolchain with rust-src.
          postInstall = ''
            wrapProgram $out/bin/ra-jit \
              --prefix PATH : ${rust}/bin \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath guiLibs}
          '';
          meta.mainProgram = "ra-jit";
        };
        # Default build: one derivation per cargo unit, rustc called directly (nix/units.nix, units.json).
        units = import ./nix/units.nix { inherit pkgs rust guiLibs; };
        ra-jit-units = pkgs.runCommand "ra-jit" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
          mkdir -p $out/bin
          makeWrapper ${units.root}/bin/ra-jit $out/bin/ra-jit \
            --prefix PATH : ${rust}/bin \
            --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath guiLibs}
        '';
        mkLife = bin: pkgs.writeShellScriptBin "ra-jit-life" ''
          exec ${bin}/bin/ra-jit life "$@"
        '';
        life = mkLife ra-jit-units;
        life-cargo = mkLife ra-jit;
      in {
        packages = {
          default = life;
          ra-jit = ra-jit-units;
          # Fallback: the old whole-crate `cargo build` (buildRustPackage).
          default-cargo = life-cargo;
          ra-jit-cargo = ra-jit;
        };
        apps.default = {
          type = "app";
          program = "${life}/bin/ra-jit-life";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rust
            pkgs.python312
            pkgs.git
            pkgs.pkg-config
            pkgs.gnused # vendor.sh uses GNU `sed -i`
            pkgs.coreutils
          ];
          # vendor.sh copies ra_ap_hir_ty from the cargo registry; `cargo fetch` populates it.
        };
      });
}
