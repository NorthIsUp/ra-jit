# Changelog

All notable changes to ra-jit are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `vendor/rust-analyzer` and `vendor/rustc_abi` are git subtrees instead of submodules: a
  plain `git clone` builds, and the flake builds from its own tree with no second pin of the
  forks.

### Fixed

- The code of a function grows in a straight line with the arms of a match that leave
  through `?`, as `#[derive(Serialize, Deserialize)]` writes for an enum: it grew with
  their square, 329 MB for one function of 285 arms, which an AArch64 branch cannot reach
  across ([#5](https://github.com/indexable-inc/ra-jit/issues/5)). `harness/growth.py`
  checks it.

## [0.1.0] - 2026-10-20

The first release.

### Added

- `ra-jit`, a compiler daemon that keeps a workspace loaded in rust-analyzer and generates
  code from its MIR with Cranelift. After the first build, an edit rebuilds only the
  functions that changed.
- `cargo ra-jit build|run|test|check`, with cargo's own flags. Anything the daemon cannot
  build falls back to cargo, and says so.
- `ra-jit enable` and `ra-jit disable`: route plain `cargo build|run|test|check` through
  ra-jit, and put cargo's config back.
- `ra-jit-live` (on crates.io), a crate that applies edits to a running program, keeping
  its state (macOS on Apple silicon).
- A nix flake: `nix run github:indexable-inc/ra-jit` opens a Game of Life you can edit
  while it runs; `#ra-jit` is the cargo wrapper.
- Prebuilt binaries for Linux (x86_64, aarch64) and macOS (aarch64).
- `cargo install ra-jit` from crates.io (Cranelift; `--features llvm` needs LLVM 20). The
  rust-analyzer and rustc_abi forks it builds on are published as `ra-jit-<name>`.

### Known limits

- Rust 1.99.0 only.
- No borrow checker: keep running `cargo check`.
- Changing a struct the running program uses asks for a restart.

[Unreleased]: https://github.com/indexable-inc/ra-jit/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/indexable-inc/ra-jit/releases/tag/v0.1.0
