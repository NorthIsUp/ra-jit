# ra-jit plan

Goal: a fast dev-loop Rust compiler built on rust-analyzer (ra): one long-lived, whole-workspace, in-memory
query database, MIR lowered to Cranelift, JIT-run, changed functions hot-swapped through a fn-pointer table.
rustc + LLVM stays the release and CI compiler. See `HANDOFF.md` for what exists today and how to run it.

## Decision (2026-10-19)

Build on rust-analyzer, not rustc.

- rustc's slowness is architectural: one process per crate, incremental state reloaded from disk, an edit to a
  low crate rebuilds every dependent crate and relinks. Making rustc a whole-workspace in-memory database is a
  core rewrite.
- ra already has that architecture. Editing a fn body without changing its signature stops at that fn, across
  crates, so per-edit cost should track the size of the change, not the dependency graph.
- What ra lacks is a finite, known list of compiler features (below), and ra keeps absorbing rustc pieces as
  shared libraries (`rustc_abi` layouts, the next trait solver), so the gap shrinks on its own.
- Never replace rustc for release builds: rustc + LLVM keeps optimisation, targets and debug info.

## Correctness policy

- Accepting code rustc would reject (ra has only partial borrowck, fewer lints) is acceptable: rustc
  (`cargo check`, CI) still runs on every change.
- Miscompiles are not acceptable, even rare ones. Differential testing from day one: run the same programs
  (and generated ones) under ra-jit and native rustc and diff the output, as the sim already does.

## Evidence so far (dev-aws-8, single crate)

| edit | ra-jit edit-to-swap | `cargo build` incremental |
|---|---|---|
| toy fib/tick | 2.9 ms | 157 ms |
| sim constant or rule | 7.3 ms | 169 ms |
| 300 fns in another file | 11.3 ms | 205 ms |
| fn inside the 300-fn file | 58 ms | 277-282 ms |

Cold start about 3.4 s (mostly ra analysing `core`). JIT cost tracks the edited file's size: ra re-lowers every
function in the edited file (section below).

## Next steps, in order

1. **Multi-crate chain benchmark** (settles the monorepo claim): generated workspace of ~50 crates in a chain,
   edit a fn body near the bottom, ra-jit vs `cargo build`. Expect ra-jit flat, cargo growing with the chain.
2. **Per-function invalidation** in ra, so cost tracks the edited fn, not the file. See the section below.
3. **Differential testing harness**: ra-jit vs rustc on a corpus plus generated programs; every divergence is a
   ra MIR bug to fix (no overflow checks or bounds `Assert`s today, i128 discriminants, pretty-print panics).
4. **Language coverage**: `dyn` and vtables, then closures, heap via a host allocator for `alloc`, then `std`
   and `fmt` (needs `dyn`), drops, unwinding, real i128.
5. **Lazy compilation**: compile a callee on first call through a stub instead of the eager worklist.
6. **Scale**: cold start and memory on a big workspace; persisting ra's database to disk.
7. **Hyperion**: the fork's own features (`fn_effects`, `#[nonblocking]`, type reflection) would need adding
   to ra before Hyperion code can run on this.

## Why ra re-lowers per file, not per function

Root cause: every query that reads a fn's syntax goes `AstId -> ast_id_map(file) -> SyntaxNodePtr -> parse(file)`;
both are whole-file values that change on every keystroke, so salsa re-runs every such query in the file, and
equality cutoff stops the change only one level later, after the lowering work. (Paths: ra_ap_* 0.0.358 crates,
upstream `crates/<name>`; salsa 0.28.5.) Chain for an edit inside `bar`'s body, N fns in the file:
- `set_file_text` sets the `FileText` input (LOW durability for local files, base-db/src/change.rs:94); salsa never
  compares input values, a set always bumps the revision (salsa input.rs:183-212).
- `EditionedFileId::parse` (base-db/src/editioned_file_id.rs:22, lru 128): full reparse. `Parse::reparse`
  (syntax/src/lib.rs:153) exists but the db never calls it (only ide typing.rs, fuzz). New tree: no cutoff.
- `HirFileId::ast_id_map` (hir-expand/src/lib.rs:1754): ids are stable above and inside items since #19837 (hash of
  kind+name+parent, span/src/ast_id.rs:1-21), but the map stores `(SyntaxNodePtr, id)` with absolute ranges and
  `PartialEq` compares them (ast_id.rs:602-623), so it changes. `real_span_map` (span_map.rs:53) likewise.
- `file_item_tree_query` (hir-def/src/item_tree.rs:140) re-runs; `ItemTree` has ast ids, names, visibility,
  item-anchored attrs, no ranges or bodies (item_tree.rs:209, 568): compares equal, def map not re-run.
- `FunctionSignature::with_source_map` (hir-def/src/signatures.rs:630) reads `loc.source(db)` (src.rs:16, `to_node` via
  ast_id_map + parse): re-runs for all N fns; its source map (absolute ptrs) changes for fns below the edit.
  `FunctionSignature::of` (:625) re-runs and compares equal. `GenericParams::of` is not a query, it reads
  `FunctionSignature::of` (hir/generics.rs:217); `AttrFlags::query` (attrs.rs:602) also re-runs per fn. This is
  `guest_fns`' 16-18 ms (`has_body`, `type_or_const_params`, impl signatures, plus the one file parse/item tree).
- `Body::with_source_map` (hir-def/src/expr_store/body.rs:89, lru 512): same, and `Expander::new` eagerly reads the
  file's span map and ast id map (expr_store/expander.rs:45-50). Re-runs for all N. `Body` (body.rs:40) holds only
  arenas and ids, no ranges; ptrs live in `BodySourceMap`. `Body::of` (:137) re-runs, compares equal for N-1 fns.
- `InferenceResult::for_body` (hir-ty/src/infer.rs:1094) and `mir_body_query` (hir-ty/src/mir/lower.rs:2276) never read a
  source map (`MirSpan` is ExprId-based, mir.rs:1142): for untouched fns they are only deep-verified, not re-run.
  Upstream pins this in hir-ty/src/tests/incremental.rs:95-220 (edit `bar`: `with_source_map`/`of` re-run for all 3
  fns, `InferenceResult::for_body` for 1). So "MIR 18 ms" is N body re-lowerings plus walking every infer/MIR edge.
Prototype's own share: `update` (src/main.rs:1955-1963) re-prints and rehashes MIR of every fn in the edited file
(~9 ms) and re-keys all 409 units (~4 ms). Real, but smaller than ra's re-lowering (~34 ms).

Fixes, smallest first:
1. Prototype "body-only edit" fast path (~150 lines in main.rs, low risk, falls back to today's path). Keep the last
   parse; compare per-item green nodes (offset-free: rowan `GreenChild` stores `rel_offset`). If only fn items
   differ, with identical text outside the body, not `const fn`, no `impl Trait` return, no items/impls/macro_rules
   in the body: keep the cached fn list (skip `guest_fns`), re-query `mir_body` only for those fns and any new
   instances, keep all other slots. Salsa is lazy, unqueried memos cost nothing. Expect one parse + one fn,
   independent of N (unmeasured).
2. Vendored ra patch, "item-local firewall" (~300 lines, medium risk; vendor ra_ap_hir_def + ra_ap_hir_expand). New
   tracked `item_green(FunctionId) -> GreenNode` (cheap, equal unless the fn's own text changed). `Body::of` and
   `FunctionSignature::of` lower from `SyntaxNode::new_root(item_green)` and drop the source map; the `Expander`
   reads the span map / ast id map lazily (real files need neither for hygiene, expander.rs:58-64), only for macro
   calls and item-holding blocks, shifting ptrs by the item offset. Deps are recorded dynamically, so macro-free fns
   stop depending on the file; fns with `println!` keep today's cost. Assert in debug both paths give equal `Body`.
3. Upstream-quality: item-relative source maps (offset applied in accessors) and per-item ast id maps with
   per-parent counters, so macro calls also go item-local. ~1-2k lines across span/hir-expand/hir-def/hir/ide, high
   risk. Upstream did the analogous steps for items (#19837 stable ast ids, #22322 modules, item-anchored spans,
   span_map.rs:63) but the test above accepts per-file body re-lowering; no issue for it found.

### How ra caches

- Each query run records the queries it read (edges) plus `verified_at`/`changed_at`. Setting an input bumps the
  revision (salsa runtime.rs:296) and marks durabilities up to the input's as changed (runtime.rs:231-239).
- On read in a new revision: shallow check, no input of this durability changed since `verified_at`
  (maybe_changed_after.rs:371-385; why HIGH-durability sysroot memos are O(1)); else deep verify walks the edges in
  order (:590-640) and re-executes if one changed (:247-262). Pull-based: unqueried memos are never touched.
- Early cutoff: the re-run value is compared with the old by `PartialEq` (unless `no_eq`, salsa-macros
  tracked_fn.rs:103-115); if equal, `changed_at` is backdated (function/backdate.rs:15-27), so dependents verify
  without re-running. Not when durability dropped or inside a cycle (backdate.rs:37-49).
- `lru = N` drops the value, keeps the edges (function/memo.rs:90-97); a dep with no old value reports "changed"
  (maybe_changed_after.rs:247), hence ra's LRU'd `with_source_map` behind an un-LRU'd `of` firewall.
- All in memory: no fingerprints, no content hashes, no disk (salsa's `persistence` feature is not enabled by ra,
  hir-def Cargo.toml). rustc's incremental instead stores 128-bit fingerprints of results and the dep graph on disk
  between sessions and compares fingerprints, not values.
