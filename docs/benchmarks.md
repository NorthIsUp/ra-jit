# Benchmarks

The build table in the README comes from here. It is measured by CI on one dedicated machine,
on real projects at pinned commits, against plain cargo on the same machine, and written back
to the README by a bot commit. This page says how, so that anyone can check it or run it again.

## What is measured

`harness/bench.py` copies each project's pinned checkout and builds it two ways side by side:
`cargo ra-jit build` (the daemon) and plain `cargo build` into a target directory of its own.
Both build the same source after every edit, one after the other, so they see the same machine
state.

| Phase | What happens | ra-jit | cargo |
|---|---|---|---|
| Cold | build from an empty target directory and an empty ra-jit store; crates are already downloaded | the build returns (cargo builds the first time) and then the daemon loads the workspace in the background; both are reported | `cargo build` |
| Nothing changed | build again with no edit | | |
| Edit a function | one statement is added to a function in the main crate | | incremental |
| Edit a widely used function | one statement in a function many places call: in a library crate of the workspace when there is one (ripgrep's `grep-matcher`, axum's `axum-core`, nushell's `nu-path`, bat's library), otherwise in a module the rest of the crate uses (fd, hyperfine, just) | | incremental |
| Change a signature | a method gets another parameter and every caller passes it | | incremental |
| Edit a generic function | a statement in a generic function used at many types (ripgrep and nushell) | | incremental |
| Runtime | (ripgrep) the program ra-jit linked, cargo's debug build and cargo's release build run the same search over the toolchain's own library source | | |

The edits do not change what the program does. After the first edit of each kind the program
ra-jit linked is run and has to print what cargo's prints (exit status, stdout and stderr); in
the runtime phase all three programs have to print the same. A run in which any program differs
is not published (`benchreport.py` refuses it and the workflow fails).

Each edit is made and taken back `--runs` times (5 in CI). The README shows the median of the
edits; this page also shows the fastest and slowest run of each side, and the raw JSON keeps
every sample, the time of taking each edit back, who built each step (the daemon, or cargo and
why), whether the daemon spliced the change into the program or linked it again, how many
functions it compiled, the daemon's peak memory and the daemon's own timeline of each build.
Cold is one run of each.

An edit the daemon hands to cargo (a fallback: ra-jit always builds what cargo would) is measured
once, marked `(c)` in the README, and shows about cargo's time.

## Projects

Pinned by tag and commit in `harness/bench.py` (`PROJECTS`); the clone has to be at that commit
or the run stops.

| Project | Tag | Commit | Run to check the output |
|---|---|---|---|
| ripgrep | 15.1.0 | `af60c2de9d85` | `rg -n --no-heading --sort path "fn main" ...` |
| fd | v10.3.0 | `d38148f0aabd` | `fd --extension rs main src` |
| bat | v0.26.0 | `c734087e1d15` | `bat --plain --color=never rustfmt.toml` |
| hyperfine | v1.19.0 | `12fec4209864` | `hyperfine --help` |
| just | 1.43.0 | `b7e7c46495b3` | `just --list --unsorted` |
| axum | axum-v0.8.6 | `c1bb9c3e9062` | built (`-p example-hello-world`), not run: it is a server |
| nushell | 0.108.0 | `da141be11b3a` | `nu -n -c "[3 1 2] \| sort \| str join ','"` |

`cargo` 0.92.0 is in `PROJECTS` too but not in the CI run (it doubles the run's length).

## Fairness

- Same machine, same source, same moment: the two builds alternate after every edit.
- Same toolchain on both sides: Rust 1.99.0 (`harness/toolchain`), from the flake's
  `devShells.bench`, which also pins LLVM 20 for the LLVM tier.
- Cargo is warm and incremental: the dev profile's default (`CARGO_INCREMENTAL` unset).
- Same linker: the toolchain's default (`cc`, which runs GNU ld on the benchmark machine). No mold,
  no lld, no sccache or other `RUSTC_WRAPPER` on either side (bench.py removes it).
- Crates are downloaded before the cold build on both sides; downloading is not timed.
- ra-jit's store starts empty for each project and backend; no warm store from another checkout.
- The machine runs nothing else: it is a dedicated instance used only by this workflow.

## Tiers

Each project is measured with each code producer ra-jit has: Cranelift (the default, in the
README) and LLVM (`RA_JIT_BACKEND=llvm RA_JIT_OPT=2 RA_JIT_EMIT=fast`). Cranelift is measured
with ra-jit as it is installed and released, without the `llvm` feature (`RA_JIT_BIN`), and LLVM
with a build that has it (`RA_JIT_LLVM_BIN`): a binary that links libLLVM loads it each time it
starts, about 6 ms on the benchmark machine, and every `cargo ra-jit` starts it once. A new tier
is one more entry in `BACKENDS` in `harness/bench.py`; the report makes a table for every
backend in the result.

## The machine

Every result records its machine: the name it was given (`RA_JIT_BENCH_MACHINE`), the CPU, the
cores the run could use (it may be pinned with `taskset`) and the machine's total, memory,
kernel, rustc, C compiler, linker and LLVM version (below and in the JSON).

The CI machine is a dedicated AWS instance used for nothing else, registered as a self-hosted
GitHub Actions runner with the labels `self-hosted, linux, x64, bench-x86`. It runs as user
`andrew` from `~/actions-runner` under a user systemd unit (`ra-jit-bench-runner.service`,
lingering, so it starts at boot); `harness/benchrunner.sh` sets all of that up, and its `.env`
names the machine for the results. The instance meant for it is `dev-aws-bench-x86`
(i-01666717f0de39201, us-west-2a, on demand, ttl a year, tagged `ix-dev-purpose=ra-jit-bench`):
16 AMD EPYC 9R45 (Zen 5) cores, one thread per core, NixOS. It was launched as a c8a.4xlarge
(32 GiB), but nushell's daemon alone holds 15 GB, so it should be resized to m8a.4xlarge (the
same 16 cores, 64 GiB) before it takes the runner. A cloud VM is not bare metal, but its cores
are not shared with other tenants; the spread columns show how much the numbers move.

The first published run (2026-10-20) was made by hand, before that runner existed, on
`dev-aws-11`: a bare-metal c8a.metal-24xl (96 of the same Zen 5 cores, 188 GiB) that was idle
and claimed for the run, with the benchmark pinned to cores 0-15 (`taskset -c 0-15`), so that
cargo and the daemon had 16 cores as on the CI machine.

## When it runs and what it commits

`.github/workflows/bench-readme.yaml` runs on every push to main that touches code (not docs,
the site or the README), every night at 09:17 UTC, and by hand (`gh workflow run bench-readme
--repo indexable-inc/ra-jit`, with `-f force=true` to write the README even when nothing moved).
One run at a time; a push during a run waits, and a newer push replaces the waiting one.

After the run `harness/benchreport.py` compares the result with the stored one
(`docs/benchmarks/linux-x86_64.json`). It writes the README section, this page's "Latest run"
and the JSON only when:

- a median of either side, in any phase, moved by more than 10% and more than 50 ms,
- a step the daemon built is now built by cargo, or the other way round,
- a project or backend was added or removed, or
- the stored run is more than 24 hours old (so the commit the README names stays within a day
  of main).

The workflow then commits those three files to main as `github-actions[bot]` with `[skip ci]`
in the message (so the commit starts no other workflow), rebasing onto main if main moved during
the run. It uses the job's `GITHUB_TOKEN` with `contents: write`; main has no branch protection.

## Run it yourself

On Linux x86-64 with Nix (or with Rust 1.99.0, rust-src, LLVM 20 and Python 3 installed):

```
nix develop .#bench
cargo build -p ra-jit --release
CARGO_TARGET_DIR=target-llvm cargo build -p ra-jit --release --features llvm
python3 harness/bench.py ripgrep fd --runs 5 --out /tmp/bench.json   # any of the projects
python3 harness/benchreport.py /tmp/bench.json --check               # what would change
```

`--backend cranelift` measures one tier. Projects are cloned once into `RA_JIT_PROJECTS`
(default `~/ra-jit-projects`). The whole CI set (seven projects, both tiers, five runs) took 38
minutes on 16 Zen 5 cores, plus the build of ra-jit. It needs about 20 GB of memory (nushell)
and 30 GB of disk.
`python3 harness/bench.py --compare OLD.json NEW.json` lists what got slower.

## Caveats

- Live edits (the patch applied to a running program) are not measured here: they work on macOS
  on Apple silicon only. `harness/live_latency.py` measures them there.
- One machine, one OS. The numbers on a laptop will differ; the ratios are what carries over.
- Cold is one run, so it is the least precise number here.
- An edit is a small, fixed change. A larger change (many functions, a new type) costs more;
  `harness/agents.py` makes random edits of the kinds coding agents make.
- axum's output is not checked (it is a server); its build is.
- ripgrep's runtime is one short workload (tens of milliseconds, over 16 threads), so it says more
  about start-up and directory walking than about generated code; the program ra-jit linked has
  had the edits spliced into it and taken back, as a program in use would.
- Cold for `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet), so it matches
  plain cargo; the daemon then loads the workspace in the background ("Daemon ready after").
- The C compiler comes from Nix (gcc 16) with the wrapper's hardening turned off, as a
  distribution's gcc behaves; it is the same for both sides.

## Earlier one-off measurements

Edit, then build, on a workspace of 200 generated crates (`harness/genws.py`), Linux x86-64,
8 CPUs, both warm and incremental, same linker, median of three. Measured once by hand before
this page existed; not updated by CI.

| Edit | cargo | ra-jit | |
|---|---:|---:|---:|
| A function in the main crate | 1.76 s | 0.42 s | 4.2x |
| A function in a crate everything depends on | 6.33 s | 0.60 s | 10.6x |
| Functions in 30 crates at once | 6.26 s | 0.62 s | 10.1x |
| A new function | 2.09 s | 0.56 s | 3.7x |
| A signature used in every crate | 13.87 s | 10.87 s | 1.3x |

## Results

<!-- bench:start -->
### Latest run

ra-jit [`c8f58ac7d9`](https://github.com/indexable-inc/ra-jit/commit/c8f58ac7d9fb8c970b36f57f39ed34332b03bcef), 2026-10-20T23:50:27Z, Rust 1.99.0, 5 runs of each edit. Raw: [benchmarks/linux-x86_64.json](benchmarks/linux-x86_64.json). Measured by hand with `harness/bench.py`; the `bench-readme` workflow re-runs it on the dedicated benchmark machine.

| Machine | |
|---|---|
| Name | AWS c8a.metal-24xl, bare metal, pinned with taskset |
| CPU | AMD EPYC 9R45 96-Core Processor |
| Cores | 16 |
| Memory (GiB) | 188 |
| Kernel | 6.18.53 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 20.1.8 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |

#### Edits, Cranelift

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 453 ms | 50 ms | 9.1x | 450 ms to 456 ms | 49 ms to 85 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 724 ms | 37 ms | 20x | 723 ms to 727 ms | 37 ms to 64 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 562 ms | 68 ms | 8.3x | 557 ms to 564 ms | 68 ms to 72 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 704 ms | 40 ms | 18x | 696 ms to 1.19 s | 39 ms to 58 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 386 ms | 19 ms | 20x | 381 ms to 390 ms | 18 ms to 66 ms | daemon |  |
| fd 10.3.0 | Edit a function | 434 ms | 43 ms | 10x | 431 ms to 435 ms | 43 ms to 54 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 400 ms | 32 ms | 12x | 396 ms to 403 ms | 32 ms to 37 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 638 ms | 71 ms | 9.0x | 440 ms to 803 ms | 66 ms to 107 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 385 ms | 22 ms | 18x | 379 ms to 391 ms | 22 ms to 75 ms | daemon |  |
| bat 0.26.0 | Edit a function | 435 ms | 45 ms | 9.7x | 433 ms to 438 ms | 45 ms to 247 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 678 ms | 45 ms | 15x | 675 ms to 1.03 s | 34 ms to 78 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 948 ms | 53 ms | 18x | 629 ms to 1.08 s | 52 ms to 57 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 58 ms | 16 ms | 3.6x | 57 ms to 59 ms | 15 ms to 162 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 318 ms | 34 ms | 9.4x | 314 ms to 319 ms | 34 ms to 42 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 350 ms | 34 ms | 10x | 348 ms to 353 ms | 33 ms to 35 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 355 ms | 71 ms | 5.0x | 354 ms to 359 ms | 70 ms to 74 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 300 ms | 21 ms | 14x | 297 ms to 308 ms | 20 ms to 45 ms | daemon |  |
| just 1.43.0 | Edit a function | 662 ms | 38 ms | 17x | 656 ms to 676 ms | 38 ms to 83 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 700 ms | 37 ms | 19x | 686 ms to 1.22 s | 37 ms to 47 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 677 ms | 62 ms | 11x | 660 ms to 1.25 s | 61 ms to 63 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 40 ms | 14 ms | 2.9x | 40 ms to 42 ms | 14 ms to 76 ms | daemon |  |
| axum 0.8.6 | Edit a function | 350 ms | 50 ms | 7.0x | 348 ms to 354 ms | 48 ms to 56 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 814 ms | 43 ms | 19x | 805 ms to 817 ms | 43 ms to 44 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 71 ms | 16 ms | 4.4x | 71 ms to 73 ms | 16 ms to 88 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.44 s | 261 ms | 5.5x | 1.39 s to 1.58 s | 245 ms to 530 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.80 s | 231 ms | 21x | 4.79 s to 4.85 s | 228 ms to 233 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.85 s | 308 ms | 16x | 4.80 s to 7.97 s | 273 ms to 375 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.53 s | 186 ms | 35x | 6.51 s to 6.71 s | 177 ms to 233 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 178 ms | 26 ms | 6.8x | 178 ms to 186 ms | 22 ms to 9.37 s | daemon |  |

#### Cold, Cranelift

From an empty target directory and an empty store, crates already downloaded. One run.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready after | Functions compiled | Daemon memory |
|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 4.45 s | 4.49 s | 4.81 s | 17,103 | 2,623 MB |
| fd 10.3.0 | 20.52 s | 20.49 s | 4.27 s | 8,157 | 2,235 MB |
| bat 0.26.0 | 9.29 s | 9.04 s | 7.97 s | 11,353 | 2,827 MB |
| hyperfine 1.19.0 | 4.42 s | 4.40 s | 5.83 s | 5,762 | 2,131 MB |
| just 1.43.0 | 6.20 s | 6.10 s | 6.14 s | 21,033 | 2,950 MB |
| axum 0.8.6 | 6.71 s | 6.75 s | 5.32 s | 5,515 | 2,490 MB |
| nushell 0.108.0 | 28.50 s | 28.18 s | 24.67 s | 160,178 | 16,743 MB |

#### How fast the program runs, Cranelift

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 34 ms (34 ms to 35 ms) | 33 ms (33 ms to 33 ms) | 10 ms (10 ms to 10 ms) |

#### Edits, LLVM

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 450 ms | 78 ms | 5.8x | 447 ms to 453 ms | 77 ms to 93 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 726 ms | 48 ms | 15x | 720 ms to 1.05 s | 47 ms to 71 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 559 ms | 105 ms | 5.3x | 555 ms to 563 ms | 103 ms to 110 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 697 ms | 57 ms | 12x | 693 ms to 1.00 s | 57 ms to 62 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 384 ms | 28 ms | 14x | 383 ms to 387 ms | 28 ms to 71 ms | daemon |  |
| fd 10.3.0 | Edit a function | 440 ms | 66 ms | 6.7x | 439 ms to 833 ms | 63 ms to 77 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 405 ms | 44 ms | 9.2x | 400 ms to 407 ms | 44 ms to 48 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 565 ms | 82 ms | 6.9x | 440 ms to 618 ms | 79 ms to 126 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 389 ms | 31 ms | 13x | 379 ms to 390 ms | 31 ms to 81 ms | daemon |  |
| bat 0.26.0 | Edit a function | 431 ms | 83 ms | 5.2x | 425 ms to 433 ms | 82 ms to 250 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 678 ms | 47 ms | 14x | 672 ms to 1.11 s | 45 ms to 90 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 1.07 s | 62 ms | 17x | 635 ms to 1.08 s | 55 ms to 65 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 58 ms | 25 ms | 2.3x | 58 ms to 59 ms | 25 ms to 158 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 316 ms | 51 ms | 6.2x | 312 ms to 318 ms | 50 ms to 60 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 351 ms | 44 ms | 8.0x | 348 ms to 353 ms | 44 ms to 46 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 353 ms | 138 ms | 2.6x | 351 ms to 356 ms | 136 ms to 140 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 300 ms | 30 ms | 10x | 298 ms to 302 ms | 30 ms to 56 ms | daemon |  |
| just 1.43.0 | Edit a function | 662 ms | 53 ms | 12x | 655 ms to 672 ms | 52 ms to 90 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 686 ms | 49 ms | 14x | 682 ms to 1.19 s | 45 ms to 89 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 677 ms | 88 ms | 7.7x | 667 ms to 1.16 s | 88 ms to 98 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 41 ms | 24 ms | 1.7x | 40 ms to 41 ms | 23 ms to 137 ms | daemon |  |
| axum 0.8.6 | Edit a function | 349 ms | 57 ms | 6.1x | 346 ms to 351 ms | 56 ms to 62 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 807 ms | 53 ms | 15x | 806 ms to 811 ms | 51 ms to 53 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 73 ms | 26 ms | 2.8x | 70 ms to 79 ms | 25 ms to 96 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.46 s | 478 ms | 3.1x | 1.42 s to 1.48 s | 461 ms to 558 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.80 s | 218 ms | 22x | 4.76 s to 4.84 s | 218 ms to 222 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.89 s | 297 ms | 16x | 4.85 s to 7.15 s | 257 ms to 373 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.49 s | 414 ms | 16x | 6.42 s to 8.79 s | 410 ms to 463 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 183 ms | 44 ms | 4.2x | 180 ms to 185 ms | 34 ms to 4.52 s | daemon |  |

#### Cold, LLVM

From an empty target directory and an empty store, crates already downloaded. One run.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready after | Functions compiled | Daemon memory |
|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 4.49 s | 4.54 s | 6.23 s | 17,103 | 2,682 MB |
| fd 10.3.0 | 20.65 s | 20.58 s | 4.75 s | 8,157 | 2,270 MB |
| bat 0.26.0 | 9.26 s | 9.09 s | 8.93 s | 11,353 | 2,912 MB |
| hyperfine 1.19.0 | 4.45 s | 4.45 s | 6.09 s | 5,762 | 2,206 MB |
| just 1.43.0 | 6.14 s | 6.19 s | 7.62 s | 21,033 | 3,091 MB |
| axum 0.8.6 | 6.75 s | 6.78 s | 5.55 s | 5,515 | 2,481 MB |
| nushell 0.108.0 | 27.85 s | 28.31 s | 41.44 s | 160,181 | 15,412 MB |

#### How fast the program runs, LLVM

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 33 ms (33 ms to 33 ms) | 34 ms (34 ms to 34 ms) | 10 ms (10 ms to 10 ms) |

<!-- bench:end -->
