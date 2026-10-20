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

Edit, then build (generated 200-crate workspace, Linux x86-64, 8 CPUs, both warm and
incremental, same linker, median of three):

| Edit | cargo | ra-jit | |
|---|---:|---:|---:|
| A function in the main crate | 1.76 s | 0.42 s | 4.2x |
| A function in a crate everything depends on | 6.33 s | 0.60 s | 10.6x |
| Functions in 30 crates at once | 6.26 s | 0.62 s | 10.1x |
| A new function | 2.09 s | 0.56 s | 3.7x |
| A signature used in every crate | 13.87 s | 10.87 s | 1.3x |

Real projects (64-core Linux x86-64, median of 5, same linker; [details](docs/results.md#11-real-projects)):

| Project | Build from nothing: cargo / ra-jit ready | Edit in a library crate: cargo / ra-jit |
|---|---:|---:|
| ripgrep 15.1.0 | 6.2 s / 13.0 s | 0.79 s / 0.08 s (10x) |
| just 1.43.0 | 8.1 s / 16.8 s | 0.92 s / 0.07 s (14x) |
| nushell 0.108.0 | 35.5 s / 75.9 s | 6.19 s / 0.24 s (26x) |

The first build takes about twice as long as `cargo build`, because ra-jit compiles every
program once more after cargo. Not measured yet: workspaces of a million lines or more,
signature and struct-field edits on real projects, and test suites
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

## What to know

- Live edits work on macOS Apple silicon only. Elsewhere you still get fast builds.
- No borrow checker: keep running `cargo check`.
- Rust 1.99.0 only.
- Changing a struct the program is using asks for a restart.

More: [how it works](docs/architecture.md), [full results](docs/results.md),
[what is supported](docs/support.md).

## Credits

ra-jit started as a fork of [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), thanks to
its authors. It has moved well past it, so use ra-jit rather than rustd. Built on
[rust-analyzer](https://github.com/rust-lang/rust-analyzer). Not endorsed by either project.

Made at [ix](https://ix.dev) for den's native app. MIT or Apache-2.0.
