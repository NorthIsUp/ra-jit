# Tiers

A program that takes edits with `ra-jit-live` is also made faster while it runs. The daemon
samples where its threads are, compiles the functions it spends its time in again with LLVM,
and swaps them in as patches, the way HotSpot or V8 tier up, without a restart
([#6](https://github.com/indexable-inc/ra-jit/issues/6)). macOS on Apple silicon only, as
live edits are; it needs a ra-jit built with `--features llvm`.

![The Tiers island of the Game of Life window](tiers.png)

## The levels

| | Name in code and UI | What compiles it | What it is for |
|---|---|---|---|
| 1 | `Level::Cranelift`, "Cranelift" (`CL`) | Cranelift, one function at a time | every build and every edit: about 5 ms from the save to the running program |
| 2 | `Level::Llvm`, "LLVM -O3" (`O3`) | LLVM at `-O3`, with what the function calls built in where that is hot or small | a hot function, about a second after it gets hot; it counts its branches |
| 3 | `Level::Pgo`, "LLVM -O3 + PGO" (`PGO`) | the same IR again, with what level 2 counted given to LLVM as `!prof` | a level-2 function that has counted for a second |

The names are `ra_jit_live::format::LEVELS`, which the daemon's events and the window share.

## How it goes

1. **The program samples itself.** The worker that links a program's executable writes
   `sample` in the program's directory (`~/.cache/ra-jit/live/<uuid>/`). The program's
   `ra-jit-live-sample` thread then looks at one busy thread in turn every 2 ms
   (`thread_get_state`, which stops the thread while it reads), counts the address it was
   at if that is the program's own code or a patch's, and writes `profile` four times a
   second: the counts since it started, the CPU it took itself, and the counters level-2
   code counts in. Without `sample` the thread only looks for it, every 250 ms.
2. **The worker puts each address in a function** (`crates/ra-jit/src/tier.rs`): the
   executable's own symbol table, or, for an address in the room for patches, the function
   a live patch put there in place of the executable's (`Baseline::live` says where each
   went). It keeps a second of samples per function.
3. **It asks for the hot ones.** A function with 2% of the samples in the last second, and
   at least 10 of them, is asked for at level 2, four at a time at most. A function at level
   2 that has counted for a second and was entered at least 50 times is asked for at level 3.
   Every function that has had 0.3% of the samples is one that may be built into what is
   compiled. Code that is not the daemon's (rustc's copies of the dependencies) is shown at
   level 0 and is not asked for again.
4. **The worker compiles them beside its builds** (`compile_tiered` in `main.rs`, with
   `hir_ty::mir::codegen::tier::compile`) on a thread of its own with a copy of the
   database, keeps the code with the program (`Rejections::tiers`), and builds a patch as an
   edit's is built. A tiered function differs from the last full build, so the patch has it
   at its level; it is spliced into the executable and published to the program, which
   writes a jump over the old entry. The program writes nothing it was not already writing.
5. **It says what it did**: `tiers` in the program's directory (`format::Tiers`, read with
   `ra_jit_live::tiers()`) has each hot function's level, share of the samples and what is
   under way, and the last events; the worker's log has the same events as `tier:` lines.

### What is built in

Cranelift's code calls every function but the smallest of the standard library. At level 2
the IR is built again with the calls of a hot function, or of any function whose built body
is at most 600 Cranelift instructions, replaced by the body (`tier::inline_limit`,
`tier::hot_body`), up to 10 deep and 40 000 instructions in all. That crosses crates: the
standard library's iterators, slice indexing and its `ub_checks`, closures, and
`#[track_caller]` functions (their body takes the caller's location as the call passed it).
`#[inline(never)]` stays a call, as rustc keeps it. Without this LLVM has nothing to work
with: per-function `-O3` of the benchmark below runs at 50 generations per second, 2.5
times Cranelift, where level 3 runs at 528.

### Profile-guided optimization

Level-2 code counts how often it is entered and, for each two-way branch of its IR in the
order of the layout, how often it went each way: `select(condition, a, b)` picks the counter,
so a branch costs one increment. The counters are `ra_jit_live_counters()`, a 64 K array in
every program that links `ra-jit-live`, called by name; the worker hands out ranges of it.
Level 3 builds the same IR (the same functions built in), finds the same branches in the
same order, and gives LLVM `!prof !{"branch_weights", taken, not}` on each and
`!{"function_entry_count", n}` on the function; it counts nothing. When the branches do not
line up (they always have so far), level 3 is compiled without weights.

### Edits and tiers

An edit wins. A tiered function's code is kept with the print of the Cranelift code of the
function and of every function built into it. The build after an edit asks for those prints
(Cranelift compiles the edited function anyway, which is what it would have done), and a
function whose print or one of whose built-in functions' prints changed is at level 1 again
in that same build: the edit is not held up by LLVM. The worker tells the watcher, which
starts the function over; it is compiled again once it is hot again, typically a second
later. A full build (a type changed, a link) sends every function back to level 1.

### What is not replaced

- A frame that is running a function finishes in the code it started in; the next call runs
  the new code. There is no on-stack replacement, so a loop that never returns from the
  function it is in is never sped up. The Life demo calls `step()` once a generation.
- Shims (drop glue, vtable and `FnOnce` shims) stay at level 1.
- A function a caller of level 2 or 3 has built in is not called from there any more, and
  an edit of it demotes the caller too.

### The store

Code of level 2 or 3 is never put in the store: it depends on the profile and on what was
hot, which no key of an entry names. It is kept in the worker for as long as the program
runs, and a new daemon starts every function at level 1.

## Turning it off

`RA_JIT_TIER=off` in the daemon's environment starts none of it: no program is asked to
sample, no function leaves level 1, and the worker says `tier: off` in its log.
`RA_JIT_TIER=off` in a program's own environment keeps it from sampling.
`RA_JIT_TIER=2` stops at level 2, which then counts nothing. `RA_JIT_DUMP_TIER=1` has the
worker print the IR of each tiered function and why a body was not built in.

## Measured

`demo/lifebench` (the Game of Life of `demo/life` on a 512 × 512 grid, run flat out) on an
M-series Mac under load from other builds, 2026-10-20, in runs of 25 seconds; steady state is the mean of the half-second rates from 15 s on.

| Build | Generations per second | vs release |
|---|---|---|
| level 1, Cranelift (`RA_JIT_TIER=off`) | 21 | 1.9% |
| rustc debug (`cargo build`) | 20 | 1.8% |
| every function LLVM `-O3`, nothing built in (`RA_JIT_BACKEND=llvm RA_JIT_OPT=3`) | 50 | 4.4% |
| level 2, LLVM -O3 (`RA_JIT_TIER=2`) | 482 | 43% |
| level 3, LLVM -O3 + PGO (default) | 528 | 47% |
| rustc `-O3` with `overflow-checks` and `debug-assertions`, the semantics of the dev profile | 890 | 79% |
| rustc release (`cargo build --release`) | 1130 | 100% |

- **Time to each level:** `neighbours` at level 2 at 0.6 s and `step` at 1.1 s; level 3 at
  1.6 s and 2.2 s; the rate is steady from about 3 s. A compile at level 2 or 3 takes 2 to
  50 ms (`step` with 155 functions built in: 35 ms, then 26 ms).
- **While level 2 counts** (about a second), the rate is 50 to 80 generations per second:
  the counting is in the hot loop. Level 2 without counting (`RA_JIT_TIER=2`) is at 480
  within 1.5 s.
- **Edits:** from the save to the new executable, 6.7 ms (median of 6, 6.4 to 9.2) with
  tiering off and 7.8 ms (7.5 to 9.0) with `step`, `neighbours`, `next_age` and `fold`
  tiered; the program applies the patch in 0.25 to 0.33 ms and 0.4 to 0.8 ms (the patch
  carries the tiered functions). An edit that arrives while a function is being compiled at
  a higher level waits for it: 18 and 27 ms in that run.
- **Sampler:** 0.8 to 0.9% of one core at 500 Hz with one or two busy threads, 1.3 to 1.5%
  at 1000 Hz.
- **Output:** what the tiered program prints is cargo's, checksum for checksum
  (`harness/tiers.py`).

### Where the rest of the gap is

The issue's mark is within 10% of a release build. Level 3 is at 47% of it, and at 59% of a
release build that checks overflow and debug assertions as the dev profile does, which the
tiered code has to keep. What is left, as far as it was looked at:

- LLVM gets the IR from Cranelift's, where every address is an integer turned into a pointer
  where it is used and nothing says what may alias: no `noalias` or `dereferenceable` on a
  reference, no types to tell memory apart. A store to the new grid makes LLVM read the
  fields of `self.age` again in the loop.
- The size of a body is counted before any optimization, where every local goes through the
  frame, so bodies rustc would build in are left as calls.

## Checked by

`harness/tiers.py` (macOS on AArch64; it says so and exits 0 elsewhere): promotion to levels
2 and 3 with output identical to cargo's, an edit of a tiered function that demotes it and
is applied at once, and `RA_JIT_TIER=off` changing nothing.
