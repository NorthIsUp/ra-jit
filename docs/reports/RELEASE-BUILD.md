# How the release binaries are built

Branch `build-profile`, from main at 40ddac92ab, 2026-10-21. The question was whether ra-jit is
built the fastest way it can be for the daemon's edit loop and its cold start: the allocator,
the Cargo profile, and profile-guided optimization (PGO). What a release ships now: the `dist`
profile (`release` with thin LTO), optimized with a profile of its own work, built by
`.github/scripts/build_release.py`. Against the plain `release` build it replaces, on Linux:
the cold start 14% shorter on nushell and on ripgrep, an edit of a function 8% shorter on
nushell, the same peak memory.

## How it was measured

`harness/abbench.py` (new) runs builds of ra-jit against each other on `bench.py`'s edits of a
real project, in rounds whose order rotates, each build with its own copy of the project, home
and store; a turn starts a daemon from nothing (the cold start: cargo's first build and the
wait until the daemon is ready), makes every edit three times and takes it back. Every number
is a median over rounds x 3 samples, against the first build of the table; a bracket is the
bootstrap 95% interval of the change, given where it matters. The first round's cold start
also checks every dependency with cargo and is left out of the cold-start medians.

- Linux: dev-aws-5, c8i.16xlarge (32 cores, 64 threads, 480 MiB L3), THP `madvise`. Every
  experiment pinned to cores 0-15 (`taskset`), builds to the other half or run before.
  ripgrep 15.1.0 and nushell, Cranelift.
- macOS: an M1 Max shared with other agents' builds (load 30 to 48), so wall times there are
  noise; the daemon's workers' CPU cycles (`proc_pid_rusage`) and lifetime peak footprint are
  what is reported from it.

"Edit work" is the daemon's `edit-to-binary` less the link: what the daemon computes for an
edit of nushell, about 220 ms of a 270 ms edit. On ripgrep an edit is mostly the link and
`cargo`, and the profile does not move it outside the noise.

## Where an edit's time goes

`perf record` of the workers during nushell edits (mimalloc v3, release): salsa revalidating
memos (`maybe_changed_after`, `verify_memo`, `try_claim`) is 73% of user cycles; the allocator
1.8%, `memcpy`/`memmove` 0.5%, drops 0.8%. So no allocator can make an edit more than about 2%
faster, and none did.

## Allocator

Linux, 4 rounds, against mimalloc v3 (what the `mimalloc` 0.1.52 in `Cargo.lock` builds by
default):

| allocator | nushell edit work | nushell cold start | nushell peak | ripgrep cold start | ripgrep peak |
|---|---|---|---|---|---|
| mimalloc v3 | 229 ms | 25.9 s | 15.4 GB | 4.96 s | 2.58 GB |
| mimalloc v2 | +2% | +7% | -1% | +5% | -12% |
| mimalloc v3, large OS pages | -3% | +3% | +1% | 0% | +1% |
| jemalloc (tikv-jemallocator) | +1% | +7% | -10% | +5% | -21% |
| jemalloc, background threads | -2% | +8% | -10% | +5% | -18% |
| snmalloc | +3% | +12% | +28% | +6% | +19% |

On the Mac: peak memory mimalloc v2 -17% (nushell) and -16% (ripgrep), jemalloc -8% and -7%,
snmalloc +18% and +30%; nushell edit cycles v2 +6%, jemalloc -1%.

Kept: mimalloc v3, the fastest cold start; no allocator changes an edit outside the noise.
jemalloc holds 10-21% less memory for a 5-8% longer cold start, which is the trade to make
if memory ever matters more than the cold start. snmalloc is worse at everything.

## Cargo profile and PGO

The final run (E), from main at 40ddac92ab with the worker fix below, 5 rounds. `dist` is
`release` with `lto = "thin"`; the PGO builds are `build_release.py`'s, the instrumented build
and the optimized one of the same profile:

| build | nushell edit of a function | nushell edit work | nushell cold start | ripgrep cold start | ripgrep first edit | peak |
|---|---|---|---|---|---|---|
| release | 267 ms | 225 ms | 25.4 s | 5.01 s | 1600 ms | 15.7 GB, 2.6 GB |
| dist (thin LTO) | -5% | -4% [-9, 0] | -5% | -3% | -9% | +1%, -1% |
| **dist + PGO** | **-8%** [-14, -2] | **-5%** [-10, -3] | **-14%** | **-14%** | **-16%** [-20, -2] | 0%, +1% |
| release + PGO | -7% | -2% | -12% | -13% | -10% | 0%, 0% |

Every one of dist + PGO's cold starts was shorter than every one of release's: nushell 21.8 to
22.3 s against 25.0 to 26.9 s, ripgrep 4.3 s against 4.9 to 5.2 s.

The runs before it, from main at 1f28e9877b, on nushell's edit work and cold start:

| build | B (3 rounds) | C (5 rounds) | D (5 rounds, quiet host) |
|---|---|---|---|
| thin LTO, 16 codegen units | -7%, -4% | -6%, -6% | -5%, -4% |
| thin LTO, 1 codegen unit | -7%, -8% | -5%, 0% | |
| fat LTO, 1 codegen unit | -5%, -8% | | |
| `-Ctarget-cpu=x86-64-v3` | -1%, -3% | | |
| PGO, release | -3%, -14% | | 0%, -11% |
| PGO from the release-profile instrumented build, on thin LTO, 1 codegen unit | -9%, -12% | -3%, -5% | |
| the same, 16 codegen units | | -5%, -4% | -3%, -3% |

In these runs the profile came from an instrumented `release` build whatever profile it was
used on, and on thin LTO it gave most of both wins once (B) and little of either twice (C, D;
C ran beside builds on the other cores). E's builds take the profile from an instrumented
build of the profile they optimize, as `build_release.py` does, and got both. On the Mac (B
and C's builds, cycles), thin LTO took 4-7% fewer cycles to cold-start nushell and PGO 6%
(the one interval clear of zero); edit cycles moved within the noise (-3% to +6%).

Build time of ra-jit (`cargo build -p ra-jit`, Linux, 32 cores):

| profile | from clean | after a one-line change in `crates/ra-jit` |
|---|---|---|
| release | 73 s | 18 s |
| thin LTO, 16 codegen units (`dist`) | 83 s | 46 s |
| thin LTO, 1 codegen unit | 241 s | 118 s |
| fat LTO, 1 codegen unit | 427 s | 307 s |
| `build_release.py` (dist twice, training between) | 319 s | |

Adopted: thin LTO with the default codegen units, as the `dist` profile, with PGO. One
codegen unit and fat LTO cost three to six times the build for nothing thin LTO did not
already give. `release` stays as it is: the harnesses, CI and every agent build it again
after each change, and thin LTO makes that rebuild 2.5 times longer for 4-5% of edit work.

## PGO, as a release is built

`build_release.py --target T` builds `dist` instrumented (`-Cprofile-generate`), runs it on
fd, bat, just and hyperfine (projects the benchmarks do not measure) through
`abbench.py --train` (a cold start and every kind of edit, twice), merges the counts with the
toolchain's `llvm-profdata` (`llvm-tools`), refuses a profile under 10^9 counts, and builds
`dist` again with `-Cprofile-use`. The release workflow runs it in place of its `cargo build`,
on each target's own runner (all three are native), so there is one way release binaries
are made. It costs about four times the plain build: 319 s against 72 s on the dev host, an
estimated 25 to 30 minutes on a hosted runner, which is why the job's limit is now 120
minutes.

What it took:

- A killed process writes no counters. `cargo ra-jit stop` kills the workers, so
  `--train` ends a turn by killing the daemon, after which each worker sees its input close
  and exits by itself; each process writes its own file (`LLVM_PROFILE_FILE=%p`), and the
  merge waits until no instrumented process is left. With one shared file, a spare still
  writing it as the merge read it crashed `llvm-profdata`.
- On main, a worker that loaded its workspace beside cargo's first build never ended when
  its daemon died (below), so the training waited 120 s per project for nothing and got a
  quarter of the counts: 13 against 57 billion.
- On macOS the `cc` crate passes `-Cprofile-generate` on to Apple clang, whose profile format
  is not rustc's LLVM's, and the instrumented ra-jit crashed at start
  (rust-lang/rust#163640): the C code (mimalloc, ring) is built with
  `-fno-profile-generate`/`-fno-profile-use`. GCC on Linux accepts the same flags.

The nix flake and `cargo install` build `release` and get neither thin LTO nor PGO.

## Rejected

- **`panic = "abort"`**: the worker catches panics (`catch_unwind` around a build and its
  preparation), and salsa cancels a query by unwinding. Not tried.
- **`opt-level = 3`**: already `release`'s default.
- **`-Ctarget-cpu`**: x86-64-v3 moved nothing measurable (-1% edit work), and a shipped
  binary has to run on any x86-64 and AArch64 machine.
- **One codegen unit, fat LTO**: above.
- **BOLT**: not tried. PGO already lays out the hot code; BOLT would add a Linux-only third
  build step and `llvm-bolt`, which `llvm-tools` does not include.

## Found on the way: workers that outlived their daemon

A worker started ahead of cargo's first build (#11) reads cargo's lines on a thread with
`for line in stdin().lines()`, and on the last one calls `end_with_input`, which reads the
standard input until it closes. `lines()` holds the `StdinLock` for the whole loop, so that
read waited for a lock its own thread held, forever. A daemon that crashed or was killed
left such a worker running with its memory (2 GB on fd) and its socket, and the next daemon's
first build fell back to cargo ("another daemon serves"). Fixed in its own commit.
`cargo_ra_jit.py`'s daemon checks (run alone, macOS): without the fix "the workers of the
killed daemon are gone" and the two checks after it fail, with it they hold. The checks of
that part that fail with and without it: 16 clients at once running the same program while
the daemon splices it (on macOS one of them sticks in `_dyld_start` or is killed, signal 9),
and the three checks of a client of another build of ra-jit replacing the daemon.

## Verified

macOS, the `dist` build with the fix, `RUST_BACKTRACE` unset: `difftest.py corpus --layouts
--mixed 8` and `mixfutures --mixed 16 --layouts` match, `growth.py` and `movedlines.py` pass,
`cargo test --release -p ra-jit` passes, `cargo test --release -p hir-ide` fails the 18 tests
it fails on main and no other. `build_release.py` ran through on Linux x86-64 (57 billion
counts, 319 s) and on macOS AArch64 (64 billion counts, 512 s on the loaded Mac), and the
corpus with `--mixed 8` matches under the macOS binary it made. Not run: Linux AArch64,
Windows (not a release target), the LLVM backend.
