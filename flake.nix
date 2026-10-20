{
  description = "ra-jit: a compiler daemon and code generator, with live edits";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    # vendor/rust-analyzer is a submodule, which a github: flake does not fetch: the same
    # commit comes in as an input and is put in place below.
    rust-analyzer = {
      url = "github:DioxusLabs/rustd/6b1940be0e68cb8004301d6c2dcbdf3f71257fca";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, rust-analyzer }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        # The one toolchain ra-jit is pinned to (harness/toolchain); rust-src is the sysroot
        # source rust-analyzer reads `core` from.
        rust = pkgs.rust-bin.stable."1.99.0".default.override {
          extensions = [ "rust-src" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rust;
          rustc = rust;
        };
        src = pkgs.runCommand "ra-jit-src" { } ''
          cp -r ${self} $out
          chmod -R u+w $out
          rmdir $out/vendor/rust-analyzer 2>/dev/null || true
          cp -r ${rust-analyzer} $out/vendor/rust-analyzer
        '';
        daemon = rustPlatform.buildRustPackage {
          pname = "ra-jit";
          version = "0.0.0";
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "ra-jit" ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          doCheck = false;
          # ra-jit, cargo-ra-jit and ra-jit-rustc run each other from next to themselves; the
          # toolchain comes along on PATH.
          postInstall = ''
            for bin in ra-jit cargo-ra-jit ra-jit-rustc; do
              wrapProgram $out/bin/$bin --prefix PATH : ${rust}/bin
            done
          '';
          meta.mainProgram = "cargo-ra-jit";
        };
        # `nix run github:indexable-inc/ra-jit -- build` is `cargo ra-jit build`.
        run = pkgs.writeShellScriptBin "ra-jit" ''
          export PATH=${daemon}/bin:${rust}/bin:$PATH
          exec ${daemon}/bin/cargo-ra-jit ra-jit "$@"
        '';
      in {
        packages = {
          default = run;
          inherit daemon run;
        };
        apps.default = {
          type = "app";
          program = "${run}/bin/ra-jit";
        };
        devShells.default = pkgs.mkShell {
          packages = [ rust pkgs.python3 ];
        };
      });
}
