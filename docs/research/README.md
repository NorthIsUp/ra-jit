# Research

Findings, one file per topic, dated, each claim marked **read** (in a source we read, or
measured), **reported** (someone says so) or **unverified** (our inference). What was fetched to
back them is under [docs/upstream/](../upstream/), each with a `SOURCE.md`. Search here first.

- [rust-compiler-2026.md](rust-compiler-2026.md): the Rust compiler's 2026 (next solver,
  Polonius, LLVM 23, move elimination, polymorphization, cargo's build-dir) and what ra-jit
  does about each. 2026-10-20.
- [mir-gap.md](mir-gap.md): ra-jit's MIR against rustc's optimized MIR, match rates, the
  passes ported and not, and the production gaps ranked. 2026-10-20.
- [borrowck.md](borrowck.md): rustc's NLL borrow checker and Polonius (the nightly-default
  alpha, the legacy datalog analysis, stabilization status, the UI tests that tell them apart),
  rust-analyzer's removed borrowck, and what ra-jit builds. 2026-10-20.
