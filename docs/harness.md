# The harnesses

ra-jit is tested by comparison with rustc. Almost every script under `harness/` builds
something with cargo and rustc, builds it again with ra-jit's code in place of rustc's for
some crates, runs both, and compares standard output, standard error and exit status. A
difference is a failure. So is a function ra-jit refused, in the harnesses that count them.

There are also Rust unit tests in `crates/ra-jit` and in the code generator; the harnesses
are what carries the weight.

## Running them

You need Python 3, Rust 1.99.0 with `rust-src`, a C compiler and linker, and a release build
of ra-jit. Some harnesses need more: `debuginfo.py` needs gdb and `llvm-dwarfdump`,
`buildmodel.py` needs the `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`
targets with their cross tools and qemu, `remote.py` needs a MinIO or SeaweedFS binary,
`storefull.py` needs passwordless `sudo` to mount a tmpfs.

    cargo +1.99.0 build -p ra-jit --release
    cd harness
    export RA_JIT_BIN=$PWD/../target/release/ra-jit

`RA_JIT_BIN` names the ra-jit under test and defaults to `../target/release/ra-jit`. The
scripts pick the toolchain from `harness/toolchain` unless `RUSTUP_TOOLCHAIN` is set. They
also default `RUSTFLAGS` to `-C link-dead-code`, which `cargo ra-jit` does not use, and
`CARGO_INCREMENTAL` to 0. Scratch directories go under `TMPDIR`; several of the harnesses
need a few gigabytes there.

### The gate

This is the list CI runs on every push, in `.github/workflows/ra-jit.yaml`. A change is not
done until all of it exits 0 under both backends.

    python3 difftest.py corpus --bin runner --crate cases --layouts
    python3 programs.py
    python3 stress.py
    python3 breakers.py
    python3 unwinds.py
    python3 growth.py
    python3 splices.py
    python3 asmedits.py
    python3 includes.py
    python3 worktrees.py
    python3 keys.py
    python3 fuzz/fuzz.py smoke --backends $RA_JIT_BACKEND     # x86-64 only
    python3 debuginfo.py                                      # Linux only
    python3 buildmodel.py                                     # Linux only; runs both backends itself
    python3 storekills.py --kills 100
    python3 storecorrupts.py --rounds 4
    python3 storefull.py                                      # Linux only
    python3 storeshared.py
    python3 remote.py                                         # needs MinIO or SeaweedFS

On AArch64 add `RA_JIT_SPLICE=1 python3 farsplices.py`. On macOS CI also runs
`worktrees.py --join --together`, `worktrees.py --overlay` and, under Cranelift, `tiers.py`.

Alongside the harnesses, CI runs `cargo fmt --all -- --check` and
`cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p ra-jit`.

### Under Cranelift

Cranelift is the default. Either leave `RA_JIT_BACKEND` unset or set it:

    export RA_JIT_BACKEND=cranelift

### Under LLVM

Build with the `llvm` feature against LLVM 20, then select the backend. The gate is run at
`-O2` with fast instruction selection, and that is the only LLVM configuration it covers.

    LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo +1.99.0 build -p ra-jit --release --features llvm
    export RA_JIT_BACKEND=llvm RA_JIT_OPT=2 RA_JIT_EMIT=fast

A binary built with `--features llvm` still uses Cranelift unless `RA_JIT_BACKEND=llvm` is
set, so one build serves both runs. Under LLVM also run

    python3 backends.py corpus runner cases --bin runner --levels 2f

which reports how many functions LLVM compiled itself and how many it left to Cranelift,
with reasons. The daemon's log says `backend: llvm -O2 fast emit` when LLVM is in use.

## What each one checks

### Correctness of generated code

| Script | What it checks |
|---|---|
| `difftest.py <workspace> --bin <pkg> --crate <name>` | The base comparison. Builds the binary with cargo, compiles the named crates with ra-jit, links ra-jit's object ahead of rustc's rlibs, runs both. `--layouts` also compares the size and alignment of every type ra-jit laid out with rustc's `-Zprint-type-sizes`. `--bisect` links each ra-jit function in on its own to name the ones that change behaviour. `--mixed N` runs N builds with a random half of the functions left to rustc. `--memcheck` runs under valgrind |
| `harness/corpus/` | The workspace `difftest.py corpus` runs: a library with one module per area of the code generator and a runner that calls every function and prints the result |
| `breakers.py` | Every reduction under `harness/breakers/`, each run on its own so a crash in one does not hide the others. These are minimal cases of bugs found in real crates, rustc's tests and the fuzzer |
| `unwinds.py` | Edits to functions a panic unwinds through |
| `growth.py` | That the code of one function grows in a straight line with the arms of its match: the `serialize` `#[derive(Serialize)]` gives an internally tagged enum, a `?` in every arm, at 160 and 320 arms (issue #5) |
| `asmedits.py` | Edits to `asm!` blocks and the code around them |
| `fuzz/fuzz.py` | Generated programs, see below |
| `uitest.py <rust checkout>` | rustc's own `tests/ui` run-pass tests. Every file is either run or left out with a counted reason |
| `libsuite.py <rust checkout> coretests\|alloctests\|stdtests` | The standard library's test suites, cut down to what builds on stable |
| `cratetest.py <crate-dir>` | A crate's own integration tests: each test binary is relinked with ra-jit's objects and every test must report the same result |
| `cratesweep.sh`, `vendor.sh`, `sweepreport.py` | `cratetest.py` over a list of crates.io releases, and a summary |
| `bintest.py <name>` | A real program from `breakers/bins.json`: the same command lines given to cargo's binary and ra-jit's |
| `alone.py` | A program whose workspace crates rustc compiles nothing of, so no refused function can hide behind rustc's copy |
| `backends.py` | One program compiled by each code producer: counts, time per function, and what LLVM left to Cranelift |
| `scoreboard.py` | Runs the targets in `targets.json` and fails when behaviour differs or coverage drops below the committed `scoreboard.json` |

### The daemon following edits

| Script | What it checks |
|---|---|
| `stress.py` | 99 edits of the kinds people make (bodies, signatures, new items, moved lines, broken code, reverts) applied in order to a three-crate workspace under one warm daemon, each compared with `cargo build` of the same source |
| `programs.py` | Two programs of one workspace built by one daemon |
| `worktrees.py` | Several checkouts of one workspace built by one daemon, edited independently |
| `includes.py` | Edits to a file only `include!` names |
| `splices.py` | Edits that are hard on the splice: thread-locals, statics, vtables, function pointers, closures, panics, foreign calls. Counts how each step was linked |
| `farsplices.py` | Splices into a binary whose code is further from its end than an AArch64 branch reaches |
| `agents.py` | Random edits of the kind coding agents make, on a real workspace |
| `invalidation.py` | How much the database redoes after different kinds of edit |
| `movedlines.py` | A panic reports its new line after lines are added above its function (seven ways to panic, each against cargo), and moving a function compiles nothing |
| `buildmodel.py` | One small cargo project per feature of cargo's build model, under both backends, with steps that change one input at a time |
| `cargo_ra_jit.py` | `cargo ra-jit` against plain cargo on seven real projects, 22 flag combinations, and 11 daemon lifecycle checks. Needs the network and about 20 minutes; not in CI |
| `debuginfo.py` | Backtraces, `llvm-dwarfdump --verify`, gdb stopping and stepping, locals |
| `live_e2e.py` | Edits of a running program that links `ra-jit-live`, each applied as a patch. macOS on AArch64 |
| `live_latency.py` | Edit-to-running time of live edits on `harness/live`, `demo/life`, `harness/live-bevy` and `harness/live-egui`: isolated edits, bursts of valid keystrokes, real typing replayed key by key (most texts in between do not compile; a program told to restart is started again), and a session's restarts against `cargo run`. Writes p50 to p99 and every sample, with the machine and its battery, to `LATENCY_JSON`. Measures; checks nothing; macOS on AArch64 ([live.md](live.md#latency-under-typing)) |
| `tiers.py` | Tiers ([tiers.md](tiers.md)): `demo/lifebench` running under the daemon reaches levels 2 and 3 with output identical to cargo's, an edit of a tiered function demotes it and is applied at once, and `RA_JIT_TIER=off` changes nothing. macOS on AArch64 with `--features llvm`; it exits 0 elsewhere |
| `tierbench.py` | The benchmark table of [tiers.md](tiers.md): `demo/lifebench` at level 1, 2 and 3 under a daemon and as cargo builds it (release, and `-O3` with the dev profile's checks), in interleaved rounds, the steady rate of each. Measures; checks nothing; not in CI |

### The store and the remote

| Script | What it checks |
|---|---|
| `keys.py` | Whether the store ever hands a build code compiled from something else. One case per thing code depends on besides its file's text |
| `storekills.py` | The daemon killed at random moments while writing its store |
| `storecorrupts.py` | Packs damaged on disk, and stores of other formats |
| `storefull.py` | A store on a disk with no room |
| `storeshared.py` | Several daemons and `ra-jit cache` commands on one store at once |
| `remote.py`, `s3fake.py` | A store shared by two machines through an S3 service, with the transfer broken, objects spoiled and manifests forged |
| `deps.py` | cargo's builds of registry crates pushed and pulled. Builds offline from cargo's registry cache, so it does not run on a clean CI machine |

### Timing

`bench.py`, `editloop.py`, `edittimes.py`, `scaling.py`, `dxedits.py`, `linkbench.py`,
`shapes.py`, `heap.py`, `fnincr.py`, `realbench.py` and `live_latency.py` measure and do not gate. `fnincr.py` counts
the queries an edit inside one function of a big file runs (the file comes from `genbig.py`),
against the same functions in many small files ([reports/INCREMENTAL.md](reports/INCREMENTAL.md)). `realbench.py` times
`cargo ra-jit build` against `cargo build` on ripgrep, just and nushell (cold, no-op and
three kinds of edit); the numbers are in [results.md](results.md#11-real-projects). Their docstrings say how to run them.
`bench.py` on real projects, with `benchreport.py`, is also what writes the README's build
table: [benchmarks.md](benchmarks.md). Besides the edits it times repeated cold builds against
fresh cargo with an empty and with a warm store (`--cold-runs`), the same steps under the
fastest cargo setups (`--setups`: mold, wild, `-Zthreads=8`, cg_clif and `cargo check`) and the
test suites under both (`--tests`). `hotpatch.py` times an edit of a running program under
Dioxus's subsecond against `cargo ra-jit build` and `cargo build` with a restart; its program
is `harness/hotpatch`. `benchhistory.py` appends a run to `docs/benchmarks/history/` (what the
site's /bench page charts) and, with `check`, fails when a median moved past its noise band.
`live_latency.py` times live edits on macOS on AArch64, per keystroke under typing (`replay`,
`burst`) as well as one edit at a time (`socket`), and the restarts of a session against `cargo
run` (`restart`); its JSON goes to `benchhistory.py live`, and the numbers measured on a
MacBook are in [live.md](live.md#latency-under-typing).

## The fuzzer

`fuzz/fuzz.py` builds generated programs with cargo and with ra-jit under each backend and
compares what they print. `fuzz/fuzz.py smoke` is part of the gate. How to run longer
runs, what a finding is and what the fuzzer does not cover are in
[fuzzing.md](fuzzing.md).

## Adding a corpus case

The corpus is for things the code generator must handle that are small enough to state as
a function. Add a `pub fn` to the module for its area in
`harness/corpus/cases/src/lib.rs`, or a new `pub mod`. Call it from
`harness/corpus/runner/src/main.rs` inside `case!(...)`, which prints the expression and its
value. The result has to be deterministic and `Debug`. Then run

    python3 difftest.py corpus --bin runner --crate cases --layouts

It must print `MATCH`, and the count of functions from ra-jit must go up by what you added,
with none left to rustc.

## Adding a breaker

A breaker is the reduction of a bug found somewhere else. Add a file
`harness/breakers/cases/src/<name>.rs` with a `//!` comment at the top that says where the
bug was found and what the cause is, and a `pub fn run()` returning something `Debug`. Add
`<name>` to the `cases![...]` list in `harness/breakers/cases/src/lib.rs`. Then

    python3 breakers.py <name>

A breaker should fail before the fix and pass after. If you are recording a difference you
are not fixing, add the name to `OPEN` in `breakers.py`, or to `LEFT` if ra-jit refuses the
function and says so. Both lists are the project's list of known wrong or missing
behaviour, so keep the comment in the case honest about why it is open.

A bug that only shows in a crate's own tests goes in `CRATES` in `breakers.py` as a small
crate under `harness/breakers/`.

## Reproducing a failure

Start from the script's last lines. Each daemon harness prints the path of the daemon's log
and of the scratch copy of the source it was editing, for example

    99 of 99 right, 0 restarts; log /tmp/ra-jit-stress-out-XXXX/daemon.log; source /tmp/ra-jit-stress-XXXX

and the stepwise ones take `--only TEXT`, `--from N` and `--keep` (see each docstring).

For a difference in generated code:

1. Run `difftest.py` on the workspace with `--bisect`. It names the functions whose ra-jit
   version changes the output, with the first differing line.
2. `ra-jit --report FILE.tsv` writes the outcome of every function (compiled, skipped and
   why). `--deny FILE` leaves the functions listed in a file to rustc, and `--isolate DIR`
   writes one object per function.
3. `RA_JIT_DUMP_MIR=<function name>` prints the MIR of functions with exactly that name
   (`{closure}` for closures). `RA_JIT_DUMP_LLVM=1` prints the LLVM module of every function
   compiled, and `RA_JIT_DUMP_CLIF=1` the Cranelift IR of drop glue.
4. Reduce it to a function, add it as a breaker, and fix it there.

For a fuzzer finding, `fuzz.py repro SEED-ROUND` rebuilds the program and
`fuzz.py check found/FILE.rs` runs a stored reduction.

For a CI failure, the last line of the harness step lists which scripts failed.
