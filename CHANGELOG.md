# Changelog

All notable changes to ra-jit are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tiers ([#6](https://github.com/indexable-inc/ra-jit/issues/6)): a program that takes
  edits with `ra-jit-live` is sampled while it runs, and the functions it spends its time in
  are compiled again with LLVM, at `-O3` with what they call built in (level 2) and then with
  the branch counts level 2 took as `!prof` (level 3), and swapped in as patches without a
  restart. An edit sends an edited function back to Cranelift in the same build.
  `RA_JIT_TIER=off` turns it off. The Game of Life window shows each hot function's level
  and the promotions as they happen; `demo/lifebench` and `harness/tiers.py` measure and
  check it ([docs/tiers.md](docs/tiers.md)).
- `ra_jit_live::tiers()`, and the sampler that the daemon turns on in a running program.
  It stops once the worker that reads it is gone.
- Benchmarks on real projects that keep the README current: `harness/bench.py` measures
  ripgrep, fd, bat, hyperfine, just, axum and nushell at pinned commits (cold build, no-op,
  four kinds of edit, and how fast ripgrep's program runs) against incremental cargo, on both
  tiers, keeping every sample; `harness/benchreport.py` writes the README table and
  [docs/benchmarks.md](docs/benchmarks.md) from the result; the `bench-readme` workflow runs
  them on a dedicated Linux x86-64 machine after pushes to main and nightly, and commits the
  numbers to main when they moved beyond noise.
- `harness/bench.py` measures what [#7](https://github.com/indexable-inc/ra-jit/issues/7)
  asks of real projects: helix, a bevy example and zed (1.7M lines) join the seven, every project gets two more
  edits (a new function, a struct field set wherever the struct is built), the cold build is
  run `--cold-runs` times against fresh cargo, each with an empty store, and as many times
  in a second checkout with the store the first filled, the store's size is kept, and `--tests`
  runs each project's test suite under `cargo ra-jit test` and `cargo test` and fails on any
  test that ends differently.
- `bench.py --setups` builds every step again with the fastest ways to build Rust there
  already are ([#8](https://github.com/indexable-inc/ra-jit/issues/8)): mold, wild,
  `-Zthreads=8`, rustc_codegen_cranelift on a pinned nightly, all three at once, and
  `cargo check`, each into its own target directory; `harness/hotpatch.py` times an edit of
  a running program under Dioxus's subsecond (`dx serve --hotpatch`) against `cargo ra-jit
  build` and `cargo build` with a restart. The bench shell has mold, wild, `dx` and the
  nightly.
- Benchmark history and regression alerts
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)): `harness/benchhistory.py`
  appends each run's medians to `docs/benchmarks/history/*.jsonl` and checks the last against
  the runs before it on the same machine; the `bench-readme` workflow commits the history on
  every run, starts the site's deploy, and on a regression past the noise band opens (or
  comments on) an issue labelled `bench-regression` and fails. The live-edit latency workflow
  runs every night on main on a macOS arm64 runner and does the same with its history.
- [ra-jit.ix.dev/bench](https://ra-jit.ix.dev/bench): the benchmark history over commits, from
  `docs/benchmarks/history`: the latest run's table per project, and charts of each edit,
  the cold build, the daemon's peak memory, the store size and live edit-to-running latency
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)).
- LLVM is given what rustc gives it: reference parameters marked `noalias`, `readonly`,
  `dereferenceable`, `align`, `nonnull` and `noundef` as rustc marks them, addresses as
  pointers rather than integers, branches threaded before `-O3`, and, at a level above 1, the
  bodies of the callees it keeps calling. Level 3 of `demo/lifebench` goes from 528 to 763
  generations per second: 68% of a release build, and 79% of one that checks overflow as
  the dev profile does ([docs/tiers.md](docs/tiers.md)).
- `harness/tierbench.py`, the benchmark table of docs/tiers.md.
- `RA_JIT_DUMP_LLVM=after` prints the LLVM IR as the passes left it.
- Live-edit latency under real typing ([#9](https://github.com/indexable-inc/ra-jit/issues/9)):
  `harness/live_latency.py replay` types scripted edits one key at a time at 30 to 100 ms a
  key, with pauses at word ends, typos and backspaces, sends the whole file on each key as an
  editor does, and times every key whose text compiles to the program running it, by patch or
  by a restart as a launcher would do one; it counts the keys that do not compile, the
  superseded edits and the restarts. `restart` times a session's struct changes against
  `cargo run`. Every table has p95 and p99, and `LATENCY_JSON` carries the machine, its
  battery, the commit and the run, which `benchhistory.py live` reads. `harness/live-bevy`
  (bevy 0.20, generic systems) and `harness/live-egui` (egui 0.37, generic widgets) are real
  programs that take edits. Measured on a MacBook Pro in
  [docs/live.md](docs/live.md#latency-under-typing).

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
- An edit that arrives while a function is being compiled at a higher level no longer waits
  for LLVM: the compile lets go of the database before LLVM runs.

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
