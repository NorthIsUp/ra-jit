# The borrow checker

ra-jit has its own borrow checker, `crates/hir-ty-borrowck`. It checks the MIR rust-analyzer
lowers, the same MIR the code generator compiles, and it is meant to give exactly rustc's
verdict: the same errors, on the same lines, and none where rustc gives none. It gives
rustc's verdict on 65% of rustc's own borrow-check tests today; how they are scored and
what is left is below.

It runs as part of rust-analyzer's diagnostics, so the daemon computes it for every edited
file, and `ra-jit borrowck` prints what it finds. The daemon does not yet refuse a build for
its errors: keep running `cargo check` for rustc's verdict.

## Modes

`RA_JIT_BORROWCK` picks one:

- `polonius`, the default: rustc's Polonius alpha, which is what nightly rustc does by
  default (`-Zpolonius=next`). It accepts more programs than NLL: a loan ends where nothing
  that can still reach it is live, which accepts the "conditional return of a borrow" cases
  NLL rejects.
- `nll`: stable rustc's checker. The only difference from `polonius` is when a loan goes out
  of scope.
- `off`.

## What rustc rules it follows

All of these are rustc's, written from `compiler/rustc_borrowck` at the pinned nightly
([research/borrowck.md](research/borrowck.md) has the sources):

- Moves and initialization: moves out of places, partial moves, maybe-uninitialized reads,
  assignment to immutable locals (E0381, E0382, E0384, E0505, E0507, E0508, E0509).
- Conflicting borrows: shared, mutable, two-phase (`vec.push(vec.len())`), shallow borrows of
  match scrutinees, and fake reads (E0499, E0500, E0501, E0502, E0503, E0506, E0524).
- Region inference: liveness of locals (drops kept live only back to where the local may be
  initialized), outlives constraints from every statement, call, cast and aggregate, the
  signature's universal regions and implied bounds (normalized parameters and the impl
  header included), where clauses (elaborated through supertraits), type tests, and
  lifetimes written in `let` annotations, casts, turbofishes and struct literal paths.
- Errors on universal regions: "lifetime may not live long enough", E0521, E0309, E0310,
  E0311, with rustc's object lifetime defaults (`&'a dyn Trait` is `dyn Trait + 'a`, a trait
  bounded by `'static` makes its objects `'static`, elsewhere in a body the lifetime is
  inferred).
- Loans that outlive their storage: E0597, E0515, E0716, and promoted constants and
  extended temporaries of `const` and `static` items as `'static`.
- Closures: captures, E0373, `FnMut` escapes, E0596 and E0594 through captures.

## How it is tested

`harness/borrowck_ui.py <rust checkout>` scores it against rustc's own UI tests at the
pinned nightly's commit: everything under `tests/ui/borrowck` and `tests/ui/nll`, every test
with a Polonius revision, and every other UI test whose expected output has a borrow-check
error. Each revision of a test is a unit. What a unit should give is what the pinned nightly
rustc gives on it, once with Polonius and once without; those runs are checked against the
tests' own `//~ ERROR` annotations so that a harness mistake shows.

A unit passes when ra-jit's set of borrow-check errors, as (error code, line), is rustc's
set. Errors rustc gives no code are named by their message. A unit is out of scope, with a
counted reason, when it cannot say anything about the borrow checker: it needs a nightly
feature, an auxiliary crate or another file, a `-Z` flag other than Polonius, or rustc
stops before borrow checking.

With no borrow checker, the 245 units rustc accepts would pass. At 8d1f6cf4df:

| Mode | In scope | Pass | rustc accepts, ra-jit too | rustc rejects, same errors |
|---|---|---|---|---|
| Polonius | 1578 | 1033 (65.5%) | 235 of 245 | 798 of 1333 |
| NLL | 1576 | 1030 (65.4%) | 217 of 226 | 813 of 1350 |

463 units are out of scope; 344 of them need a nightly feature.

The other check is that it finds nothing in code rustc accepts.
`ra-jit borrowck --project DIR` checks a cargo workspace; on ripgrep, fd, bat, hyperfine,
just and axum it finds nothing, and on nushell three errors, each a rust-analyzer lowering
difference listed below.

## What is left

The failing units by what is wrong, most first, at 8d1f6cf4df: 95 "lifetime may not live
long enough" not reported, 42 E0597 not reported, 24 E0310, 20 E0700, 18 E0381, 17 E0309,
16 E0521, then a long tail. The causes, grouped:

- **Higher-ranked regions and placeholders** (`for<'a>` bounds, closures with higher-ranked
  signatures): rustc checks them with placeholders in universes; ra-jit does not yet.
- **Closure requirements propagated to the creator**: rustc proves what a closure body needs
  of its captures' regions in the function that creates it.
- **Member constraints of `impl Trait`** (E0700, not reported yet) and opaque types'
  captured lifetimes.
- **Trait selection's region obligations**: an impl's `T: 'static` proven at an unsizing
  cast, or the region constraints of an impl a method resolves to.
- **Differences in rust-analyzer's MIR lowering** (below).

## What depends on rust-analyzer's MIR lowering

The borrow checker reads `hir-ty`'s MIR, so some verdicts depend on how that lowers. Two
changes to lowering were needed and are made in `vendor/rust-analyzer/crates/hir-ty/src/mir/lower.rs`:

- Two-phase borrows for autoref method receivers (`vec.push(vec.len())`).
- `if let ... && ...` chains branch directly, as rustc lowers them, instead of through a
  boolean temporary.

These are differences still open, each the cause of some failures:

- Match guards: rustc binds by reference while the guard runs and moves only in the arm;
  rust-analyzer binds by value first, so a guard that reads what the arm moves is reported
  (two of nushell's three errors).
- Closure captures through a reference are truncated differently from rustc (the third).
- Temporaries of a block's tail expression in edition 2021 are dropped at a different point.
- Comparisons against constant patterns take their operands by value.
- Unwind paths are not all lowered, so a few errors that only happen on unwind are missed.

## What it costs

On every edit the daemon runs the diagnostics of the edited files on a thread of its own,
beside the build, so the check adds to an edit only when it takes longer than the build.
For an edit of the corpus's largest file, the check went from a median of 40 ms with the
checker off to 52-74 ms with it on, while the build took 250-400 ms. A cold check of all of
nushell used 10-25% more CPU time. Both were measured on a loaded machine, so read them as
rough.

## Debugging

- `echo file.rs | ra-jit borrowck --edition 2021` prints a file's diagnostics as JSON.
- `RA_JIT_BORROWCK_MIR=<function>` with that command prints the MIR of a top-level function.
- `RA_JIT_BORROWCK_DEBUG=1` prints each body's locals, universal regions, constraints and
  loans.
