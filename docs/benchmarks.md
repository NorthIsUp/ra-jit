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
README) and LLVM (`RA_JIT_BACKEND=llvm RA_JIT_OPT=2 RA_JIT_EMIT=fast`). The binary is built once
with `--features llvm` and the backend is chosen by environment, so a new tier is one more entry
in `BACKENDS` in `harness/bench.py`; the report makes a table for every backend in the result.

## The machine

A dedicated AWS instance, `dev-aws-bench-x86`, c8a.4xlarge in us-west-2: 16 AMD EPYC (Zen 5)
cores, one thread per core (no SMT siblings sharing a core), 32 GiB, on demand (no Spot
reclaims), NixOS, gp3 disk. A cloud VM is not bare metal, but its cores are not shared with
other tenants; the spread columns below show how much the numbers move between runs. Its
CPU, kernel, rustc, C compiler, linker and LLVM version are recorded in each run (below and in
the JSON).

It is a self-hosted GitHub Actions runner with the labels `self-hosted, linux, x64, bench-x86`,
running as user `andrew` from `~/actions-runner` under a user systemd unit
(`ra-jit-bench-runner.service`, lingering, so it starts at boot). Its `.env` names the machine
(`RA_JIT_BENCH_MACHINE=AWS c8a.4xlarge`), which goes into each result.

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
cargo build -p ra-jit --release --features llvm
python3 harness/bench.py ripgrep fd --runs 5 --out /tmp/bench.json   # any of the projects
python3 harness/benchreport.py /tmp/bench.json --check               # what would change
```

`--backend cranelift` measures one tier. Projects are cloned once into `RA_JIT_PROJECTS`
(default `~/ra-jit-projects`). The whole CI set takes about two hours on the machine above.
`python3 harness/bench.py --compare OLD.json NEW.json` lists what got slower.

## Caveats

- Live edits (the patch applied to a running program) are not measured here: they work on macOS
  on Apple silicon only. `harness/live_latency.py` measures them there.
- One machine, one OS. The numbers on a laptop will differ; the ratios are what carries over.
- Cold is one run, so it is the least precise number here.
- An edit is a small, fixed change. A larger change (many functions, a new type) costs more;
  `harness/agents.py` makes random edits of the kinds coding agents make.
- axum's output is not checked (it is a server); its build is.
- ripgrep's runtime is one workload; the program ra-jit linked has had the edits spliced into it
  and taken back, as a program in use would.

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
<!-- bench:end -->
