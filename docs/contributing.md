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
    vendor/subtree.py         moves vendor/ changes to the forks, and the forks and upstream into vendor/
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
together. A branch that changes vendor/ merges into main like any other branch; there is
nothing to port and no pointer to bump.

The fork follows main by itself: on every push to main that changes `vendor/`, the
`fork sync` workflow runs

    vendor/subtree.py push rust-analyzer
    vendor/subtree.py push rustc_abi

which makes one fork commit for each commit on main's first-parent line that changed the
directory (its tree under `vendor/<name>`, its author, dates and message, and a line naming
it) and pushes them to the fork's branch (`ra-jit` for rust-analyzer, `main` for
rustc_abi). The same commit always makes the same fork commit, so running it by hand
(`--dry-run` makes the commits and pushes nothing) and in CI agree. It only fast-forwards:
it never force-pushes, and if the fork holds a change main does not have, it stops, pushes
nothing and says so. That happens only if someone committed to the fork directly; bring it
in with `pull` below, merge that into main, and the next push goes through. A fork pushed
from a branch before its merge, or a main whose history was rewritten since the last push,
is caught up with one commit that says what it brings, once it is checked that the fork has
nothing main lacks. The `ra-jit` branch only moves forward; do not rebase or force-push it.

`git subtree split` is not used: it cannot follow `vendor/rustc_abi` past the time it was a
submodule, and it makes different commits from a branch than from main, so the fork stopped
being a fast-forward of what main makes.

### Taking a newer fork commit

    vendor/subtree.py pull rust-analyzer
    # git subtree pull --prefix vendor/rust-analyzer https://github.com/indexable-inc/rust-analyzer ra-jit --squash

That is the whole bump: there is no second pin, and the flake builds from the same files.

### Merging upstream rust-analyzer

    vendor/subtree.py upstream rust-analyzer              # its newest weekly release tag
    vendor/subtree.py upstream rust-analyzer 2026-10-26   # a tag, a branch (master) or a commit

merges upstream's commit into `vendor/rust-analyzer` on the current branch, with a clean
tree. The base is the newest upstream commit the fork's history already has (the fork's
branch and every squashed merge on this branch), so the merge brings exactly upstream's
changes since then, as merging upstream in the fork would. A clean merge is committed;
otherwise the conflicts are left in `vendor/rust-analyzer` with the usual markers and
`git status`, and `git add` and `git commit` finish it (or `git merge --abort`). The merge's
second parent is a squashed commit of upstream's tree with `git subtree`'s
`git-subtree-split` line; when main gets the merge, `push` makes upstream's commit a parent
of the fork's commit, so the fork's history records the upstream merge too.

A change to rust-analyzer should be something rust-analyzer could take: a bug fix in MIR
lowering or inference, a missing piece of the language, an accessor ra-jit needs. Where
rust-analyzer's MIR differs from rustc's, the fix goes in the fork's lowering or in a pass of
its runtime MIR queries, and the code generator relies on it: codegen does not compensate.
Code that is only ra-jit's belongs in `crates/hir-ty-codegen` or `crates/ra-jit`, where a
change is one ordinary commit. Fixes that are simply rust-analyzer bugs should be sent upstream, under
their rules.

### Moving to a newer rust-analyzer

1. On a branch, `vendor/subtree.py upstream rust-analyzer` and resolve the conflicts.
2. Update `Cargo.toml` and `Cargo.lock` at the root for dependencies rust-analyzer changed.
3. Build, and run the whole gate under both backends ([harness.md](harness.md)). A newer
   rust-analyzer changes inference, MIR lowering and layout queries, any of which can
   change generated code. The corpus with `--layouts`, `breakers.py` and
   `fuzz/fuzz.py smoke` are the ones that notice first.
4. Merge into `main`. The `fork sync` workflow sends the merge to the fork.

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
