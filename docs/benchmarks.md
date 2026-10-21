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
| Cold, warm store | after everything else, `--cold-runs` times: a second copy of the project with an empty target directory and a daemon of its own, but the store the measured copy filled with its cold build and edits (as a second checkout of the same project, or a teammate's store, would have) | build and daemon ready, as cold | |
| Nothing changed | build again with no edit | | |
| Edit a function | one statement is added to a function in the main crate | | incremental |
| Edit a widely used function | one statement in a function many places call: in a library crate of the workspace when there is one (ripgrep's `grep-matcher`, axum's `axum-core`, nushell's `nu-path`, bat's library), otherwise in a module the rest of the crate uses (fd, hyperfine, just) | | incremental |
| Change a signature | a method gets another parameter and every caller passes it | | incremental |
| Edit a generic function | a statement in a generic function used at many types (ripgrep and nushell) | | incremental |
| Add a function | a new function in the main crate, called from the function of "Edit a function" | | incremental |
| Add a struct field | a struct gets one more field (`_bench_edit: ()`) and every place that builds it sets it; in a library crate where there is one (nushell's `nu-protocol::Alias`, built in `nu-parser`) | | incremental |
| Tests | (`--tests`) the project's test suite, or the crates of it named in `PROJECTS`, after the edits: `cargo ra-jit test` and `cargo test`, run twice each, the second timed; every test has to end the same way under both | | |
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
Cold is `--cold-runs` runs of each (5 in CI), each with a new target directory, home and store;
the store size after it is kept too.

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
| helix | 25.07.1 | `a05c151bb6e8` | `hx --version` (grammars are not built: `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1`) |
| bevy | v0.20.0 | `b3bdd75becaf` | built (`--example breakout`), not run: it opens a window |
| zed | v1.24.4 | `aed37547d854` | `zed --help`; not in the CI run, see below |

zed (1.7 million lines in 249 crates, 1,291 crates with its dependencies) is in `PROJECTS` and
every edit of it builds, but it is not measured yet: ra-jit's daemon does not fit it in 123 GB.
On a c8i.16xlarge (2026-10-21, ra-jit `2b2f2dcd5b`) the daemon loaded the workspace (20 s),
its 1,756,350 symbols (26 s) and resolved the crates (25 s), then went from 20 GB to more
than 111 GB in 40 seconds and was stopped at a 112 GB limit before it was ready; cargo alone
builds it in a few minutes on the same machine.

`cargo` 0.92.0 is in `PROJECTS` too but not in the CI run (it doubles the run's length).

## Fairness

- Same machine, same source, same moment: the two builds alternate after every edit.
- Same toolchain on both sides: Rust 1.99.0 (`harness/toolchain`), from the flake's
  `devShells.bench`, which also pins LLVM 20 for the LLVM tier.
- Cargo is warm and incremental: the dev profile's default (`CARGO_INCREMENTAL` unset).
- Same linker: the toolchain's default (`cc`, which runs GNU ld on the benchmark machine). No mold,
  no lld, no sccache or other `RUSTC_WRAPPER` on either side (bench.py removes it).
- Crates are downloaded before the cold build on both sides; downloading is not timed.
- ra-jit's store starts empty for each project and backend and for every cold run. "Cold, warm
  store" is the one phase that starts with a store, and it is reported apart.
- The machine runs nothing else: it is a dedicated instance used only by this workflow.

## Tiers

Each project is measured with each code producer ra-jit has: Cranelift (the default, in the
README) and LLVM (`RA_JIT_BACKEND=llvm RA_JIT_OPT=2 RA_JIT_EMIT=fast`). The binary is built once
with `--features llvm` and the backend is chosen by environment, so a new tier is one more entry
in `BACKENDS` in `harness/bench.py`; the report makes a table for every backend in the result.

## The fastest setups

With `--setups` (all of them in the nightly run, `--setups-on ripgrep,nushell`), every step plain
cargo builds is built again by each of the fastest ways to build Rust there already are, each
into a target directory of its own, right after plain cargo, on the same source:

| Setup | What it is |
|---|---|
| mold | cargo with mold 3.0 as the linker (`-C link-arg=-fuse-ld=mold`) |
| wild | cargo with wild 0.10 as the linker (`-C link-arg=-fuse-ld=wild`) |
| `-Zthreads=8` | rustc's parallel front end, on the same 1.99.0 (`RUSTC_BOOTSTRAP=1`) |
| nightly | plain cargo on the nightly below, to tell the nightly's own difference from Cranelift's |
| cg_clif | rustc_codegen_cranelift (`-Zcodegen-backend=cranelift`), on the nightly: it ships with nightlies only |
| cg_clif + wild + `-Zthreads=8` | all three at once, the fastest cargo build there is |
| `cargo check` | builds no code, so there is nothing to run after it: the floor of what rustc does |

The flags go in through `--config target.'cfg(all())'.rustflags`, which cargo adds to each
project's own (helix sets `--cfg tokio_unstable` there; `RUSTFLAGS` would replace it).
`-Zshare-generics` is not a column: it is already on in every debug build. The nightly is
`rust-bin.nightly."2026-10-09"` with `rustc-codegen-cranelift-preview`, from the flake
(`RA_JIT_NIGHTLY`). A setup that cannot build a project is reported as failed and not timed
after that.

Hot-patching a running program is compared on its own (`harness/hotpatch.py`): a small program
that calls one function every millisecond through `subsecond::call`, edited 30 times by each of
Dioxus's `dx serve --hotpatch` (subsecond, which patches the running program on Linux),
`cargo ra-jit build` and incremental `cargo build`, the last two followed by starting the program
again (ra-jit patches a running program on macOS only). Each edit is timed from writing the file
to the program running the new code. It is a small program on purpose: subsecond needs the
program to call through `subsecond::call` and to keep running, which a command-line tool like
ripgrep does not.

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

## History and regressions

Every run of the workflow appends one line to `docs/benchmarks/history/linux-x86_64.jsonl`
(`harness/benchhistory.py build`), whether or not the README changed: the medians, spreads, who
built each step, memory and store size of each project and phase, the machine and the run's
link. The site's [/bench](https://ra-jit.ix.dev/bench) page charts that file, and the workflow
starts the site's deploy after it commits.

`benchhistory.py check` then compares the new line with the median of the last seven from the
same machine with the same cores. A number regressed when it is above that median by more than
its noise band and by more than a floor: the band is 10% or three times the spread the history
shows (1.4826 times the median absolute deviation), whichever is larger, and the floors are
50 ms for times, 100 MB for memory and 50 MB for the store. A step the daemon built that cargo
builds now is a regression whatever its time. On a regression the workflow opens an issue
labelled `bench-regression` with a table of what moved (or comments on the one already open),
writes the table to the run's summary and fails.

Live edits are checked the same way. `.github/workflows/live-latency.yaml` runs every night on
main on a GitHub macOS arm64 runner (`macos-15`), times socket, burst and replay edits of the
four live programs (`harness/live_latency.py`, [live.md](live.md#latency-under-typing)), appends
one line for all of them to `docs/benchmarks/history/macos-arm64-live.jsonl` and checks the p50
and p90 of each program and mode (floor 1 ms). A shared runner is noisier than the benchmark
machine; the band widens with the spread its own history shows.

## Run it yourself

On Linux x86-64 with Nix (or with Rust 1.99.0, rust-src, LLVM 20 and Python 3 installed):

```
nix develop .#bench
cargo build -p ra-jit --release --features llvm
python3 harness/bench.py ripgrep fd --runs 5 --out /tmp/bench.json   # any of the projects
python3 harness/benchreport.py /tmp/bench.json --check               # what would change
```

`--backend cranelift` measures one tier. `--cold-runs 5` repeats the cold builds, `--setups
all --setups-on ripgrep,nushell` adds the setups above, `--tests` the test suites, and
`--reference DIR` puts cargo's target directories outside the work directory.
`python3 harness/hotpatch.py --edits 30` times hot-patching (needs `dx`: the bench shell has it). Projects are cloned once into `RA_JIT_PROJECTS`
(default `~/ra-jit-projects`). The whole CI set (seven projects, both tiers, five runs) took 38
minutes on 16 Zen 5 cores, plus the build of ra-jit. It needs about 20 GB of memory (nushell)
and 30 GB of disk.
`python3 harness/bench.py --compare OLD.json NEW.json` lists what got slower.

## Caveats

- Live edits (the patch applied to a running program) work on macOS on Apple silicon only and are
  measured there by `harness/live_latency.py`, per keystroke under typing as well as one edit at
  a time: [live.md](live.md#latency-under-typing) has the numbers from a MacBook, and
  `history/macos-arm64-live.jsonl` the runs.
- One machine, one OS. The numbers on a laptop will differ; the ratios are what carries over.
- Cold is the least precise number here: five runs at most, each minutes long on the large
  projects.
- The setups compared are measured on ripgrep and nushell only (`--setups-on`): seven more
  builds of every step on every project would make the nightly run too long for one machine.
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

ra-jit [`6a7d245e47`](https://github.com/indexable-inc/ra-jit/commit/6a7d245e47830f26475e98db6e01905f117a2971), 2026-10-20T16:53:46Z, Rust 1.99.0, 5 runs of each edit. Raw: [benchmarks/linux-x86_64.json](benchmarks/linux-x86_64.json). Measured by hand with `harness/bench.py`; the `bench-readme` workflow re-runs it on the dedicated benchmark machine.

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
| ripgrep 15.1.0 | Edit a function | 447 ms | 90 ms | 5.0x | 441 ms to 452 ms | 88 ms to 116 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 713 ms | 50 ms | 14x | 712 ms to 1.08 s | 48 ms to 69 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 556 ms | 86 ms | 6.5x | 552 ms to 558 ms | 82 ms to 100 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 692 ms | 50 ms | 14x | 690 ms to 1.08 s | 49 ms to 54 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 382 ms | 28 ms | 14x | 377 ms to 386 ms | 27 ms to 71 ms | daemon |  |
| fd 10.3.0 | Edit a function | 433 ms | 99 ms | 4.4x | 427 ms to 613 ms | 98 ms to 137 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 403 ms | 86 ms | 4.7x | 397 ms to 405 ms | 84 ms to 89 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 651 ms | 122 ms | 5.3x | 434 ms to 660 ms | 109 ms to 179 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 381 ms | 31 ms | 12x | 377 ms to 386 ms | 31 ms to 79 ms | daemon |  |
| bat 0.26.0 | Edit a function | 428 ms | 60 ms | 7.1x | 421 ms to 432 ms | 58 ms to 245 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 667 ms | 54 ms | 12x | 665 ms to 1.30 s | 53 ms to 82 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 951 ms | 67 ms | 14x | 621 ms to 967 ms | 57 ms to 85 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 58 ms | 26 ms | 2.2x | 57 ms to 59 ms | 25 ms to 162 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 315 ms | 92 ms | 3.4x | 312 ms to 317 ms | 91 ms to 98 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 349 ms | 98 ms | 3.6x | 343 ms to 351 ms | 97 ms to 99 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 352 ms | 140 ms | 2.5x | 349 ms to 354 ms | 139 ms to 142 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 298 ms | 30 ms | 9.9x | 297 ms to 301 ms | 30 ms to 54 ms | daemon |  |
| just 1.43.0 | Edit a function | 656 ms | 48 ms | 14x | 650 ms to 669 ms | 47 ms to 88 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 687 ms | 52 ms | 13x | 672 ms to 1.23 s | 51 ms to 73 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 664 ms | 76 ms | 8.7x | 653 ms to 670 ms | 74 ms to 80 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 41 ms | 24 ms | 1.7x | 40 ms to 41 ms | 23 ms to 138 ms | daemon |  |
| axum 0.8.6 | Edit a function | 350 ms | 57 ms | 6.1x | 346 ms to 351 ms | 55 ms to 63 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 798 ms | 52 ms | 15x | 791 ms to 803 ms | 51 ms to 53 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 71 ms | 26 ms | 2.7x | 70 ms to 72 ms | 26 ms to 97 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.44 s | 325 ms | 4.4x | 1.40 s to 1.45 s | 316 ms to 984 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.75 s | 221 ms | 22x | 4.73 s to 4.84 s | 218 ms to 224 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.84 s | 288 ms | 17x | 4.78 s to 6.79 s | 271 ms to 380 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.48 s | 294 ms | 22x | 6.45 s to 6.54 s | 287 ms to 335 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 179 ms | 43 ms | 4.2x | 174 ms to 182 ms | 38 ms to 10.04 s | daemon |  |

#### Cold, Cranelift

From an empty target directory and an empty store, crates already downloaded. One run.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready after | Functions compiled | Daemon memory |
|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 4.45 s | 4.50 s | 4.44 s | 17,103 | 2,487 MB |
| fd 10.3.0 | 20.44 s | 20.36 s | 4.13 s | 8,157 | 2,112 MB |
| bat 0.26.0 | 9.18 s | 9.13 s | 7.73 s | 11,353 | 2,728 MB |
| hyperfine 1.19.0 | 4.38 s | 4.45 s | 5.50 s | 5,762 | 2,041 MB |
| just 1.43.0 | 6.10 s | 6.13 s | 5.79 s | 21,033 | 2,796 MB |
| axum 0.8.6 | 6.68 s | 6.76 s | 4.97 s | 5,515 | 2,415 MB |
| nushell 0.108.0 | 28.02 s | 28.21 s | 28.33 s | 160,177 | 15,282 MB |

#### How fast the program runs, Cranelift

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 35 ms (35 ms to 41 ms) | 33 ms (33 ms to 35 ms) | 10 ms (10 ms to 10 ms) |

#### Edits, LLVM

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 449 ms | 94 ms | 4.8x | 446 ms to 455 ms | 93 ms to 117 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 722 ms | 53 ms | 14x | 720 ms to 1.06 s | 50 ms to 69 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 553 ms | 107 ms | 5.2x | 550 ms to 559 ms | 105 ms to 111 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 698 ms | 63 ms | 11x | 685 ms to 1.13 s | 63 ms to 65 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 382 ms | 28 ms | 14x | 380 ms to 385 ms | 28 ms to 73 ms | daemon |  |
| fd 10.3.0 | Edit a function | 432 ms | 100 ms | 4.3x | 428 ms to 434 ms | 100 ms to 108 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 401 ms | 87 ms | 4.6x | 400 ms to 403 ms | 86 ms to 91 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 644 ms | 135 ms | 4.8x | 439 ms to 751 ms | 110 ms to 155 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 379 ms | 31 ms | 12x | 378 ms to 381 ms | 30 ms to 79 ms | daemon |  |
| bat 0.26.0 | Edit a function | 430 ms | 60 ms | 7.2x | 424 ms to 436 ms | 60 ms to 243 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 673 ms | 56 ms | 12x | 670 ms to 1.19 s | 54 ms to 106 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 934 ms | 65 ms | 14x | 625 ms to 1.01 s | 57 ms to 104 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 57 ms | 26 ms | 2.2x | 56 ms to 57 ms | 25 ms to 162 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 317 ms | 99 ms | 3.2x | 316 ms to 320 ms | 99 ms to 104 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 350 ms | 99 ms | 3.5x | 348 ms to 351 ms | 98 ms to 99 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 352 ms | 144 ms | 2.4x | 348 ms to 354 ms | 141 ms to 156 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 300 ms | 30 ms | 10x | 297 ms to 307 ms | 30 ms to 53 ms | daemon |  |
| just 1.43.0 | Edit a function | 652 ms | 48 ms | 14x | 649 ms to 668 ms | 47 ms to 87 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 684 ms | 52 ms | 13x | 675 ms to 1.23 s | 51 ms to 66 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 660 ms | 100 ms | 6.6x | 659 ms to 666 ms | 98 ms to 108 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 40 ms | 24 ms | 1.7x | 40 ms to 41 ms | 23 ms to 135 ms | daemon |  |
| axum 0.8.6 | Edit a function | 347 ms | 60 ms | 5.8x | 345 ms to 352 ms | 58 ms to 61 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 796 ms | 53 ms | 15x | 794 ms to 803 ms | 51 ms to 53 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 71 ms | 26 ms | 2.7x | 69 ms to 74 ms | 25 ms to 57 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.44 s | 338 ms | 4.3x | 1.42 s to 1.63 s | 323 ms to 756 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.79 s | 213 ms | 22x | 4.75 s to 4.82 s | 211 ms to 218 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.89 s | 279 ms | 18x | 4.83 s to 7.38 s | 269 ms to 429 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.50 s | 299 ms | 22x | 6.42 s to 6.77 s | 293 ms to 338 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 177 ms | 39 ms | 4.5x | 175 ms to 182 ms | 35 ms to 4.24 s | daemon |  |

#### Cold, LLVM

From an empty target directory and an empty store, crates already downloaded. One run.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready after | Functions compiled | Daemon memory |
|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 4.44 s | 4.53 s | 5.93 s | 17,103 | 2,554 MB |
| fd 10.3.0 | 20.59 s | 20.44 s | 4.44 s | 8,157 | 2,132 MB |
| bat 0.26.0 | 9.13 s | 9.00 s | 8.88 s | 11,353 | 2,798 MB |
| hyperfine 1.19.0 | 4.41 s | 4.45 s | 5.79 s | 5,762 | 2,084 MB |
| just 1.43.0 | 6.14 s | 6.21 s | 7.28 s | 21,033 | 2,805 MB |
| axum 0.8.6 | 6.66 s | 6.73 s | 5.28 s | 5,515 | 2,434 MB |
| nushell 0.108.0 | 27.99 s | 28.17 s | 36.86 s | 160,177 | 15,240 MB |

#### How fast the program runs, LLVM

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 34 ms (34 ms to 41 ms) | 36 ms (36 ms to 37 ms) | 10 ms (10 ms to 10 ms) |

<!-- bench:end -->
