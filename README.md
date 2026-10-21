<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/ix-mark-on-dark.svg">
    <img alt="ix" src="docs/brand/ix-mark-on-light.svg" height="40" align="absmiddle">
  </picture>
  ra-jit
</h1>

Edit Rust. See it change in milliseconds, in the program that is already running.

```
nix run github:indexable-inc/ra-jit
```

That opens a Game of Life window you can edit while it runs (live on macOS Apple silicon).

![Editing the running Game of Life: rule, colours and speed change live](docs/demo.avif)

<p align="center">
  <a href="https://ra-jit.ix.dev/updates?src=readme">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/brand/signup-on-dark.svg">
      <img alt="Tell us you use ra-jit" src="docs/brand/signup-on-light.svg" height="36">
    </picture>
  </a>
</p>

<p align="center">For release news: Watch → Custom → Releases on GitHub.</p>

## What you get

- **Live edits.** Save a file and the running program has the new code about a millisecond
  later. Its state stays. ([docs/live.md](docs/live.md))
- **Fast builds.** After the first build, an edit rebuilds only the functions you changed.
  No `rustc`, no waiting.
- **Same cargo.** `cargo ra-jit build|run|test|check` in any project.

## How fast

Live edit to running code, Game of Life demo (median of 20 edits, one 96-core Linux machine):

| Edit | incremental `cargo build` | ra-jit live | |
|---|---:|---:|---:|
| One function | 169 ms | 7 ms | **24x** |
| One function, 300 new functions added | 282 ms | 60 ms | 4.7x |

<!-- bench:start -->
Edit, then build, on real projects at pinned commits. Each cell is incremental `cargo build` → `cargo ra-jit build`, both warm, same linker, median of 5 edits. Linux x86-64, AWS c8a.metal-24xl, bare metal, pinned with taskset (AMD EPYC 9R45, 16 of its 96 cores, 188 GiB), ra-jit [`c8f58ac7d9`](https://github.com/indexable-inc/ra-jit/commit/c8f58ac7d9fb8c970b36f57f39ed34332b03bcef) on 2026-10-20.

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 453 ms → **50 ms** (9.1x) | 724 ms → **37 ms** (20x) | 562 ms → **68 ms** (8.3x) | 386 ms → **19 ms** (20x) |
| fd 10.3.0 | 434 ms → **43 ms** (10x) | 400 ms → **32 ms** (12x) | 638 ms → **71 ms** (9.0x) | 385 ms → **22 ms** (18x) |
| bat 0.26.0 | 435 ms → **45 ms** (9.7x) | 678 ms → **45 ms** (15x) | 948 ms → **53 ms** (18x) | 58 ms → **16 ms** (3.6x) |
| hyperfine 1.19.0 | 318 ms → **34 ms** (9.4x) | 350 ms → **34 ms** (10x) | 355 ms → **71 ms** (5.0x) | 300 ms → **21 ms** (14x) |
| just 1.43.0 | 662 ms → **38 ms** (17x) | 700 ms → **37 ms** (19x) | 677 ms → **62 ms** (11x) | 40 ms → **14 ms** (2.9x) |
| axum 0.8.6 | 350 ms → **50 ms** (7.0x) | 814 ms → **43 ms** (19x) |  | 71 ms → **16 ms** (4.4x) |
| nushell 0.108.0 | 1.44 s → **261 ms** (5.5x) | 4.80 s → **231 ms** (21x) | 4.85 s → **308 ms** (16x) | 178 ms → **26 ms** (6.8x) |

Measured by hand with `harness/bench.py`; the `bench-readme` workflow re-runs it on the dedicated benchmark machine. Cold builds, the spread of every number, the LLVM tier and how fast the built programs run: [docs/benchmarks.md](docs/benchmarks.md) ([raw JSON](docs/benchmarks/linux-x86_64.json)).
<!-- bench:end -->

<!-- runbench:start -->
How fast the programs run, ra-jit's LLVM build against `cargo build --release`: not published yet. On macOS arm64, where it was last tried (2026-10-20), a program whose every crate ra-jit compiles does nothing when run, so its output differs from cargo's and `benchreport.py` refuses the run. The `bench-readme` workflow fills this table from `bench.py --runtime` on the Linux x86-64 benchmark machine once every program says what cargo's says.
<!-- runbench:end -->

Not measured yet: workspaces of a million lines or more, and test suites
([#benchmarks](https://github.com/indexable-inc/ra-jit/issues?q=label%3Abenchmarks)).

## Try it on your project

```
nix run github:indexable-inc/ra-jit#ra-jit -- build
```

No nix? `cargo +1.99.0 install --locked ra-jit`, or put the binaries from a
[release](https://github.com/indexable-inc/ra-jit/releases) on your PATH. Then use Rust
1.99.0 with its source:
`rustup toolchain install 1.99.0 -c rust-src && rustup override set 1.99.0`.

### Use it as your normal `cargo build`

```
nix profile install github:indexable-inc/ra-jit#ra-jit
ra-jit enable
```

From then on plain `cargo build`, `run`, `test` and `check` in any project go through ra-jit.
`ra-jit disable` puts your cargo config back.

### Live edits

Add the crate and poll once per frame:

```toml
ra-jit-live = "0.1"
```

```rust
let mut live = ra_jit_live::Live::start().ok();
loop {
    if let Some(outcome) = live.as_mut().and_then(|it| it.poll()) {
        eprintln!("ra-jit: {outcome}");
    }
    // ...your frame
}
```

### Cached functions

`#[ra_jit::cache]` on a pure function keeps its results across runs, until an edit changes
the code it reaches ([docs/fn-cache.md](docs/fn-cache.md)).

## What to know

- Live edits work on macOS Apple silicon only. Elsewhere you still get fast builds.
- No borrow checker: keep running `cargo check`.
- Rust 1.99.0 only.
- Changing a struct the program is using asks for a restart.

More: [how it works](docs/architecture.md), [full results](docs/results.md),
[what is supported](docs/support.md).

## Credits

ra-jit stands on the shoulders of giants. Thank you to the people who built:

- [rust-analyzer](https://github.com/rust-lang/rust-analyzer), whose analysis and MIR ra-jit compiles from
- [Cranelift](https://cranelift.dev) (Bytecode Alliance), the fast code generator
- [LLVM](https://llvm.org) and [inkwell](https://github.com/TheDan64/inkwell), for the optimised tiers
- [salsa](https://github.com/salsa-rs/salsa), the incremental engine underneath
- [object](https://github.com/gimli-rs/object), for reading and writing executables
- [Rust](https://github.com/rust-lang/rust) and its compiler crates
- [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), whose code ra-jit includes

None of these projects endorses ra-jit.

Made at [ix](https://ix.dev) for den's native app. MIT or Apache-2.0.
