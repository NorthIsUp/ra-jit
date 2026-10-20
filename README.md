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

![Editing the running Game of Life: rule, colours and speed change live](docs/demo.webp)

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
Edit, then build, on real projects at pinned commits. Each cell is incremental `cargo build` → `cargo ra-jit build`, both warm, same linker, median of 5 edits. Linux x86-64, AWS c8a.metal-24xl, bare metal, pinned with taskset (AMD EPYC 9R45, 16 of its 96 cores, 188 GiB), ra-jit [`6a7d245e47`](https://github.com/indexable-inc/ra-jit/commit/6a7d245e47830f26475e98db6e01905f117a2971) on 2026-10-20.

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 447 ms → **90 ms** (5.0x) | 713 ms → **50 ms** (14x) | 556 ms → **86 ms** (6.5x) | 382 ms → **28 ms** (14x) |
| fd 10.3.0 | 433 ms → **99 ms** (4.4x) | 403 ms → **86 ms** (4.7x) | 651 ms → **122 ms** (5.3x) | 381 ms → **31 ms** (12x) |
| bat 0.26.0 | 428 ms → **60 ms** (7.1x) | 667 ms → **54 ms** (12x) | 951 ms → **67 ms** (14x) | 58 ms → **26 ms** (2.2x) |
| hyperfine 1.19.0 | 315 ms → **92 ms** (3.4x) | 349 ms → **98 ms** (3.6x) | 352 ms → **140 ms** (2.5x) | 298 ms → **30 ms** (9.9x) |
| just 1.43.0 | 656 ms → **48 ms** (14x) | 687 ms → **52 ms** (13x) | 664 ms → **76 ms** (8.7x) | 41 ms → **24 ms** (1.7x) |
| axum 0.8.6 | 350 ms → **57 ms** (6.1x) | 798 ms → **52 ms** (15x) |  | 71 ms → **26 ms** (2.7x) |
| nushell 0.108.0 | 1.44 s → **325 ms** (4.4x) | 4.75 s → **221 ms** (22x) | 4.84 s → **288 ms** (17x) | 179 ms → **43 ms** (4.2x) |

Measured by hand with `harness/bench.py`; the `bench-readme` workflow re-runs it on the dedicated benchmark machine. Cold builds, the spread of every number, the LLVM tier and how fast the built programs run: [docs/benchmarks.md](docs/benchmarks.md) ([raw JSON](docs/benchmarks/linux-x86_64.json)).
<!-- bench:end -->

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

## What to know

- Live edits work on macOS Apple silicon only. Elsewhere you still get fast builds.
- No borrow checker: keep running `cargo check`.
- Rust 1.99.0 only.
- Changing a struct the program is using asks for a restart.

More: [how it works](docs/architecture.md), [full results](docs/results.md),
[what is supported](docs/support.md).

## Credits

ra-jit includes code from [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), thanks to
its authors. Built on
[rust-analyzer](https://github.com/rust-lang/rust-analyzer). Not endorsed by either project.

Made at [ix](https://ix.dev) for den's native app. MIT or Apache-2.0.
