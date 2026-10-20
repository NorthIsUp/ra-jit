# Contributing

## AI use

This code was written largely with AI assistance under human direction. The same is true
of the harnesses, the reports under `docs/reports/` and these documents. The reports are
written in the first person by the session that did the work; "I ran" in a report means
that session ran it.

What makes that workable is the testing method. Every change is judged by comparing ra-jit's
output with rustc's on real programs, and a change that makes a harness differ does not
land. If you contribute with AI assistance, the expectation is the same as without: you
understand the change, you ran the gate, and you say what you ran and what you did not.

rust-analyzer has its own AI policy for contributions to rust-analyzer. It applies to what
you send them, including fixes you find here and want to upstream.

## Layout of the tree

    crates/ra-jit/             the daemon, the cargo drop-in, the splice, the remote
    crates/ra-jit/src/bin/     cargo-ra-jit
    crates/hir-ty-codegen/    the code generator: MIR to Cranelift IR, the producers, the store
    crates/ra-jit-wasm/        experiment: the compiler running in a web page (not in the workspace)
    harness/                  the test harnesses and their fixtures
    demo/                     small projects the harnesses and the scoreboard build
    docs/                     these documents
    docs/reports/             working reports, one per piece of work, as written at the time
    SECURITY.md               what to know before sharing a store, and how to report a problem
    vendor/rust-analyzer/     rust-analyzer with ra-jit's changes, a git subtree
    vendor/rustc_abi/         rustc's layout crate, copied from the 1.99.0 tag, a git subtree
    vendor/subtree.sh         pulls a newer fork into vendor/, pushes vendor/ changes back
    vendor/source.rev         the commit of the old development branch this tree last took from

`crates/hir-ty-codegen` is not a crate. It is the module `hir_ty::mir::codegen`, which uses
private items of `hir-ty`. It is kept outside `vendor/` because all of it is ra-jit's:
`mir.rs` in the fork declares the module with a `#[path]` that points back here. So
the fork builds only as part of this repository.

## rust-analyzer

`vendor/rust-analyzer` is a squashed git subtree of the `ra-jit` branch of
[indexable-inc/rust-analyzer](https://github.com/indexable-inc/rust-analyzer). That branch is DioxusLabs' `rust-analyzer` branch of rustd (upstream at
`dd5ed69d4b` with rustd's changes on top), merged with upstream rust-analyzer master, with
the MIR passes that make rust-analyzer's MIR like rustc's on top
([`docs/mir-passes.md`](https://github.com/indexable-inc/rust-analyzer/blob/ra-jit/docs/mir-passes.md)
there). The fork stays the public home of those changes and the place upstream is merged
in; ra-jit carries the files themselves, so a plain clone builds:

    git clone https://github.com/indexable-inc/ra-jit
    cargo +1.99.0 build -p ra-jit --release

`vendor/rustc_abi` is a subtree of [indexable-inc/rustc_abi](https://github.com/indexable-inc/rustc_abi)
(`main`) the same way.

### Changing a rust-analyzer file

Edit the file in `vendor/rust-analyzer`, build and test from the root as usual, and commit
in ra-jit like any other file: one commit can change MIR lowering and the code generator
together. Then send the vendor/ part back to the fork:

    vendor/subtree.sh push rust-analyzer
    # git subtree push --prefix vendor/rust-analyzer https://github.com/indexable-inc/rust-analyzer ra-jit

`git subtree push` splits the commits that touched `vendor/rust-analyzer` into commits of
the fork and pushes them to `ra-jit`. The `ra-jit` branch only moves forward; do not rebase
or force-push it.

### Taking a newer fork commit

    vendor/subtree.sh pull rust-analyzer
    # git subtree pull --prefix vendor/rust-analyzer https://github.com/indexable-inc/rust-analyzer ra-jit --squash

That is the whole bump: there is no second pin, and the flake builds from the same files.

A change to rust-analyzer should be something rust-analyzer could take: a bug fix in MIR
lowering or inference, a missing piece of the language, an accessor ra-jit needs. Where
rust-analyzer's MIR differs from rustc's, the fix goes in the fork's lowering or in a pass of
its runtime MIR queries, and the code generator relies on it: codegen does not compensate.
Code that is only ra-jit's belongs in `crates/hir-ty-codegen` or `crates/ra-jit`, where a
change is one ordinary commit. Fixes that are simply rust-analyzer bugs should be sent upstream, under
their rules.

### Moving to a newer rust-analyzer

1. In a clone of the fork, merge the new upstream commit into the `ra-jit` branch, resolve
   conflicts and push. Merge, not rebase, so the branch keeps moving forward. Then
   `vendor/subtree.sh pull rust-analyzer` here.
2. Update `Cargo.toml` and `Cargo.lock` at the root for dependencies rust-analyzer changed.
3. Build, and run the whole gate under both backends ([harness.md](harness.md)). A newer
   rust-analyzer changes inference, MIR lowering and layout queries, any of which can
   change generated code. The corpus with `--layouts`, `breakers.py` and
   `fuzz/fuzz.py smoke` are the ones that notice first.
4. Commit on `main`.

This was done once, on 2026-10-20, for upstream `0b21bd6596`. The merge was clean; the one
API change ra-jit had to follow was `CrateData::is_proc_macro` becoming `target_kind` (a
`base_db::TargetKind`). Step 3 has not been run on that merge yet.

### Where the history is

`main` began as a tree generated from a development branch that was rust-analyzer's tree
with ra-jit's changes made in place. That branch and its history are kept as
`ra-jit-collate` and `ra-jit-spike`. `publish/split.py` there is what wrote this layout.

The toolchain is a separate pin. ra-jit is built for Rust 1.99.0 and `vendor/rustc_abi` is
rustc's crate from that tag. Moving to another Rust version means taking the new
`rustc_abi`, rechecking everything that mirrors rustc (the Rust ABI in the code generator,
`vtable.rs`, `type_id.rs`, symbol names in `symbols.rs`), and running rustc's own test
suites through `uitest.py` and `libsuite.py` again.

## Rules for code

1. **rustc is the reference.** If ra-jit's program behaves differently from rustc's, ra-jit
   is wrong, even where rustc's behaviour looks like an accident.
2. **Never guess.** Input the code generator has no explicit lowering for is refused with a
   reason (`CodegenError`), not lowered on a best-effort basis. A refusal is visible and
   counted. A wrong guess is a miscompile nobody sees.
3. **A refused function is a bug**, to be listed and fixed, not a normal path to design
   around.
4. **General fixes.** A fix that tests for one crate, one function name or one test is not
   a fix. Find what the case is an instance of.

Style follows rust-analyzer's guide
(`vendor/rust-analyzer/docs/book/src/contributing/style.md`): `cargo fmt`, no new
dependencies without a reason, small focused changes. Comments say what the code is for and
why, not what was changed. Messages printed to the user say what happened and what ra-jit
did about it in one line.

## What a change needs

- A reduced case that fails before and passes after: a corpus function for a code
  generator feature, a breaker for a bug found in real code, a step in the relevant edit
  harness for a daemon bug. [harness.md](harness.md) says how to add each.
- The gate, under Cranelift and under LLVM, on the platform you have. Say which platforms
  you did not run. CI covers Linux x86-64, Linux AArch64, Windows and macOS.
- `cargo fmt --all -- --check` and
  `cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p ra-jit`.
- For anything that changes numbers in [results.md](results.md): the command, the commit
  and the machine, so the entry can be replaced and not just appended to.

When you report what you did, separate what you ran from what you inferred. The reports
under `docs/reports/` do this and it is the reason they can be used.

## Licence

ra-jit is MIT or Apache-2.0, like rust-analyzer. Contributions are accepted under the same
terms.
