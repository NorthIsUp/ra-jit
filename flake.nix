{
  description = "ra-jit: a compiler daemon and code generator, with live edits";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
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
        src = self;
        # The one version, from the workspace's Cargo.toml.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
        daemon = rustPlatform.buildRustPackage {
          pname = "ra-jit";
          inherit version src;
          # `ra-jit --version` names the commit (a dirty tree has none of its own).
          RA_JIT_GIT_SHA = self.shortRev or self.dirtyShortRev or "";
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
        libPath = pkgs.lib.makeLibraryPath guiLibs;

        # One derivation per cargo unit (nix/units.nix, nix/units/*.json from nix/unit_graph.py):
        # editing ra-jit's own source rebuilds only ra-jit, never the crates it depends on.
        units = json: import ./nix/units.nix {
          inherit pkgs rust guiLibs json;
          repo = ./.;
        };
        daemonUnits = units ./nix/units/daemon.json;
        lifeUnits = units ./nix/units/life.json;
        # ra-jit, cargo-ra-jit and ra-jit-rustc run each other from next to themselves, found by
        # their own path: the binaries are copied together, not wrapped or linked. The toolchain
        # has to be on PATH (the launcher puts it there).
        daemon-units = pkgs.runCommand "ra-jit-units" { } ''
          mkdir -p $out/bin
          cp -L ${daemonUnits.bins}/bin/* $out/bin/
        '';
        life-bin = pkgs.runCommand "ra-jit-life-bin" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
          mkdir -p $out/bin
          makeWrapper ${builtins.head lifeUnits.roots}/bin/life $out/bin/life \
            --prefix LD_LIBRARY_PATH : ${libPath}
        '';

        # The demo's own source (and the crate it links), copied to a writable directory at run
        # time; its crates.io dependencies are vendored from Cargo.lock so that needs no network.
        lifeSource = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [ ./demo/life ./crates/ra-jit-live ];
        };
        lifeVendor = pkgs.rustPlatform.importCargoLock { lockFile = ./demo/life/Cargo.lock; };
        # `nix run github:indexable-inc/ra-jit`: the Game of Life window. On macOS on AArch64 the
        # daemon builds the copy and edits to it show up live; elsewhere the prebuilt window runs.
        life = pkgs.writeShellScriptBin "ra-jit-life" ''
          export RA_JIT_LIFE_SOURCE=${lifeSource} RA_JIT_LIFE_VENDOR=${lifeVendor}
          export RA_JIT_LIFE_DAEMON_BINS=${daemon-units}/bin RA_JIT_LIFE_PREBUILT=${life-bin}/bin/life
          export RA_JIT_LIFE_PATH=${rust}/bin${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ":${pkgs.stdenv.cc}/bin"}
          export RA_JIT_LIFE_LIBS=${libPath}
          exec ${pkgs.python3}/bin/python3 -I ${./nix/life_launch.py} "$@"
        '';
      in {
        packages = {
          default = life;
          inherit daemon run life daemon-units life-bin;
        };
        apps.default = {
          type = "app";
          program = "${life}/bin/ra-jit-life";
        };
        apps.ra-jit = {
          type = "app";
          program = "${run}/bin/ra-jit";
        };
        devShells.default = pkgs.mkShell {
          packages = [ rust pkgs.python3 ];
        };
      });
}
