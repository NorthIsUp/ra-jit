{
  description = "ra-jit: rustd's compiler daemon and code generator, with live edits";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    # vendor/rust-analyzer is a submodule.
    self.submodules = true;
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        # The one toolchain rustd is pinned to (harness/toolchain); rust-src is the sysroot
        # source rust-analyzer reads `core` from.
        rust = pkgs.rust-bin.stable."1.99.0".default.override {
          extensions = [ "rust-src" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rust;
          rustc = rust;
        };
        rustd = rustPlatform.buildRustPackage {
          pname = "ra-jit";
          version = "0.0.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "rustd" ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          doCheck = false;
          # rustd, cargo-rustd and rustd-rustc run each other from next to themselves; the
          # toolchain comes along on PATH.
          postInstall = ''
            for bin in rustd cargo-rustd rustd-rustc; do
              wrapProgram $out/bin/$bin --prefix PATH : ${rust}/bin
            done
          '';
          meta.mainProgram = "cargo-rustd";
        };
        # `nix run github:indexable-inc/ra-jit -- build` is `cargo rustd build`.
        ra-jit = pkgs.writeShellScriptBin "ra-jit" ''
          export PATH=${rustd}/bin:${rust}/bin:$PATH
          exec ${rustd}/bin/cargo-rustd rustd "$@"
        '';
      in {
        packages = {
          default = ra-jit;
          inherit rustd ra-jit;
        };
        apps.default = {
          type = "app";
          program = "${ra-jit}/bin/ra-jit";
        };
        devShells.default = pkgs.mkShell {
          packages = [ rust pkgs.python3 ];
        };
      });
}
