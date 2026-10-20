# ra-jit

**Try it (needs [nix](https://nixos.org/download)):**

```
nix run github:indexable-inc/ra-jit
```

![Editing the running Game of Life: HighLife rule, new colours and a faster step typed into the source, each live in 2 to 3 ms (edit to running)](docs/demo.webp)

([mp4 version, 1280x720 60 fps](docs/demo.mp4))

The clip is rendered offscreen, not screen-recorded: `python3 tools/record_demo.py` (Python 3.12+, Pillow, ffmpeg) runs `ra-jit life <dir> --record out.mp4 --timeline <tsv>`, which draws the real egui window into a 1280x720 buffer at a fixed 60 fps with a software rasteriser (no OS window, no GPU) and pipes the frames to ffmpeg. The guest runs on a virtual clock (a fixed step per frame), the scripted edits in `tools/record_demo.py` are typed into the real editor, and every hot-swap time shown is measured. The script then writes `docs/demo.mp4` and a 960-wide looping `docs/demo.webp` (30 fps; `--webp-fps 60` is about 3.7 MB).

A window opens with Conway's Game of Life on the left and its Rust source on the right. Edit the source (for example change `n == 3` in `next_age` to `n == 3 || n == 6`) and the running simulation changes on the next frame, with its state intact. The header shows edit-to-running time in ms, and a toggle switches the loop to a real `cargo build` per edit so you can feel the difference.

**This is an experiment**, and a young one. The long-term goal is for this to be the whole Rust dev loop, not just a toy beside it. Today it runs a small subset of Rust (no `dyn`, closures, heap or `std` yet), and rustc + LLVM stay the compiler for release builds and CI. Differential testing against native rustc is part of the plan, because miscompiles are not acceptable.

## How it works

rust-analyzer parses and type-checks the code in one long-lived in-memory database. Its own MIR is lowered to Cranelift IR and JITed. Every call goes through a table of function pointers, and each function has a recompile key (MIR, layouts, consts, callee signatures), so an edit recompiles and swaps only the functions whose key changed. The state is owned by the host, not the guest: the guest declares `struct Life`, `fn init() -> Life` and `fn frame(l: &mut Life)`, and the host runs `init` once into memory it allocates from `Life`'s layout, then passes that memory to `frame` every frame, so state survives swaps (and the guest needs no `static mut` or `unsafe`). If an edit changes `Life`'s layout the host allocates fresh memory, re-runs `init` and shows a "state reset" note in the window instead of reinterpreting old bytes. This is an ECS-style split (guest = logic, host = owned state), so a fuller ECS API is a natural next step. Guest `static`s are still supported by the JIT (allocated once by the host, migrated field by field on a layout change), but the demo no longer uses one.

## Numbers

One run on one machine (dev-aws-8, c6i.24xlarge), medians of 20 edits, edit to swap vs incremental debug `cargo build`:

| edit | ra-jit | `cargo build` |
|---|---|---|
| one-file edit | ~7 ms | ~169 ms |
| with 300 generated functions added | ~60 ms | ~282 ms |

Cold start is about 3 s, mostly rust-analyzer analysing `core`. Raw numbers are in `docs/results/`.

## Standing on others' work

This project is mostly glue; the hard parts are other people's:

- [rust-analyzer](https://github.com/rust-lang/rust-analyzer) does the front end: parsing, name resolution, type inference, trait solving, layouts, MIR, const evaluation. Its `ra_ap_*` crates make this possible, and its authors are building the incremental architecture this relies on.
- [Cranelift](https://cranelift.dev/) (Bytecode Alliance) generates the machine code and is fast enough to make per-edit JIT sensible.
- [rustc](https://github.com/rust-lang/rust) and its `rustc_abi` / type-ir crates (shared with rust-analyzer) define the language and layouts we match.
- [egui / eframe](https://github.com/emilk/egui) draw the demo window, and [nix](https://nixos.org) with [rust-overlay](https://github.com/oxalica/rust-overlay) make it one command.

## How the nix build is cached

`nix run github:indexable-inc/ra-jit` builds every cargo unit (each crate, build script and proc-macro) as its own Nix derivation that calls `rustc` directly, not one big `cargo build`. A derivation's inputs are only that unit's sources, features, profile and the store paths of its dependencies, so after an edit to ra-jit's own source every `ra_ap_*` and Cranelift crate keeps its store path and is a cache hit (locally, and on any machine or binary cache that has built it). The old whole-crate build is still there as `nix build .#default-cargo`.

The unit list is generated, not hand-written:

```
python3 tools/unit_graph.py          # inside `nix develop`, or it calls `nix develop` itself
```

It runs `cargo build --unit-graph -Z unstable-options --release --target <triple>` once per supported nix system (x86_64-linux, aarch64-linux, aarch64-darwin, x86_64-darwin (graph only: current nixpkgs no longer supports it, so the flake does not expose it); the `TARGETS` map in `tools/unit_graph.py`, run in parallel) on the pinned nightly (`nightlyDate` in `flake.nix`, currently 2026-07-15, the last nightly before the 1.99 branch; the same toolchain compiles every unit) plus `cargo metadata`, and writes `units.json` with one graph per system under `systems.<nix system>`. Platform-specific dependencies (`num_threads` -> `libc` on apple, `inotify` on linux, ...) differ per target, which is why each system has its own graph; `nix/units.nix` picks the evaluating system's. The tool runs from any host, since `--target` needs no target std. Rerun it after changing `Cargo.toml`, `Cargo.lock` or a feature set; evaluation fails with "units.json is stale" until you do (the check covers every system, they share one `inputs` hash). `units.json` is committed (a flake consumer has no cargo at eval time). Builds are native (build scripts and proc-macros run on the machine that builds them). Limits: no metadata pipelining, so a crate waits for its dependencies' full builds, and path packages need an entry in `pathSources` in `nix/units.nix`. To support another system, add its triple to `TARGETS` and rerun.

## Not yet

Per-function invalidation in rust-analyzer (cost tracks the edited file today), a multi-crate benchmark, a differential-testing harness, `dyn`/closures/heap/`std`, lazy compilation, and scale testing. See `docs/plan.md` and `docs/HANDOFF.md`.

## Build from source

`nix develop`, then `cargo build --release` and `./target/release/ra-jit life`. Without nix: nightly-2026-07-15 with `rust-src`. `vendor/ra_ap_hir_ty` is rust-analyzer's crate with one line changed (`Operand::kind` made `pub`).

## About

Built at [ix](https://ix.dev), where we run VMs with 100+ vCPUs for exactly this kind of build-heavy work. If that sounds interesting, get in touch at andrew@ix.dev.

Licensed under MIT or Apache-2.0, at your option.
