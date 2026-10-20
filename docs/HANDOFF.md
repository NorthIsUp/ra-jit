# ra-jit handoff (2026-10-19, round 2: "slightly more complicated")

A small Rust "compiler": rust-analyzer (`ra_ap_*` 0.0.358) is the front end, its own MIR is lowered to
Cranelift 0.136, JIT-run, and only functions whose key changed are hot-swapped through a table of fn
pointers. Round 2 adds a memory model and enough of Rust to run `sim/`: a live ASCII particle simulation
(structs, an enum with `match`, arrays, `for` loops over ranges and array refs, `&mut self` methods, a
generic `clamp<T: PartialOrd>`, a `static mut WORLD`) that prints a frame every 200 ms. Editing physics or
rules in `sim/src/main.rs` changes the running sim on the next frame, and the particles keep their state.

## Results (dev-aws-8, c6i.24xlarge, idle; rustc/cargo 1.99.0; release build of ra-jit)

Summaries: `results/bench-summary-dev-aws-8.txt` (scripts `bench_sim.py`, `run_bench.sh`). dev-aws-8 dropped off
the tailnet (Spot reclaim?) right after the final run, so the per-edit raw lines and the full live frame capture
are still in `dev-aws-8:~/rajit/results/`; copy them when it is back. Each edit is applied
to the original file and then reverted; only the edits are counted (n=20 each). "edit-to-swap" = file change
noticed -> set_file_text -> MIR + keys for every fn -> Cranelift of changed fns -> finalize -> slot store.

| guest / edit | fns compiled | ra-jit edit-to-swap median (p10-p90) | of which ra MIR+keys | Cranelift | `cargo build` incremental (debug) median |
|---|---|---|---|---|---|
| sim (1 file), GRAVITY const / bounce() rule | 108 (9 guest + 99 core) | **7.3 ms** (7.2-7.6) | 5.7 ms | 0.6 ms | **169 ms** |
| sim + 300 generated fns, all in main.rs, edit one gen fn | 409 | **60.6 ms** (56-87) | 52.4 ms | 0.7 ms | **282 ms** |
| sim + 300 fns in extra.rs, edit one gen fn (in extra.rs) | 409 | **57.8 ms** (56-85) | 49.8 ms | 0.7 ms | **277 ms** |
| sim + 300 fns in extra.rs, edit sim rule (in main.rs) | 409 | **11.3 ms** (11.1-11.5) | 9.2 ms | 0.6 ms | **205 ms** |

Add ~1.4 ms for the 2 ms mtime poll (`from_mtime` column in the raw files). Max values (~27-34 ms small, ~125 ms
big) are the first edit (salsa warm-up). Round 1's integer-only toy was 2.9 ms vs 157 ms. Before generics and
`for` loops pulled ~100 core instances in, the same sim with `while` loops measured 4.4 ms / 52 ms / 54 ms /
8.3 ms (bottom of `results/bench-summary-dev-aws-8.txt`): the per-edit key work for the core instances costs ~3 ms.

Cold start (one-time): ra workspace + sysroot load ~0.47 s, first MIR + keys ~2.9 s (rust-analyzer analysing
core: def maps, lang items, trait solving), Cranelift 12 ms for the sim's 108 fns (88 ms for the 300-fn
guest), total ~3.4 s to first frame. Cold `cargo build`: 0.25 s (sim), 0.46 s (300-fn guest).

Live sample: `results/live-sim-sample.txt` (timing lines + one frame; full capture on dev-aws-8): six edits on a running
sim: GRAVITY 12 -> 60 (recompiles `Particle::step` only: the const value is in its key), `bounce()` 85% ->
110%, `Kind::weight` (feathers float up), wind added to `Particle::step`, a signature change
`bounce(v) -> bounce(v, pct)`, and a field added to `World` (static re-allocated, `ps`/`tick`/`seed` migrated
by field name, the particles do not jump). The frame counter keeps counting through all of them.

## What works now (items 1-7 of the brief)

1. **Structs, tuples, arrays, enums with real layouts.** Every type gets a `Repr`: `Zst`, `Scalar` (1 value, or
   2 for fat pointers `&str`/`&[T]`), or `Mem {size, align}` from ra's `layout_of_ty`. Mem locals and borrowed
   scalars live in Cranelift stack slots; other scalars stay in SSA variables. Field projections use
   `layout.fields.offset(i)`, enum variants use `layout.variants[..]` plus the tag (Direct and Niche
   encodings, so `Option<i64>`/`Option<&T>` work), `Rvalue::Aggregate`, `Repeat`, `Discriminant`, `Len`, tuple-
   struct/variant constructor calls (`Some(x)`). Floats (f32/f64) and casts too.
   Internal guest ABI: scalars by value, aggregates by pointer (callee copies into its own slot), aggregate
   returns through a hidden first sret pointer.
2. **References are real addresses.** `Rvalue::Ref` takes the address of the place (locals that are borrowed are
   forced into slots), `Deref` projections load the pointer, fat pointers carry their length as metadata,
   `&[T; N] -> &[T]` unsizing works. The old "shared refs by value" hack is gone; ra's `&a < &b` comparisons
   load through the refs.
3. **Fixed-size arrays** with constant and runtime indexing; ra emits no bounds-check `Assert`, so the JIT adds
   one (trap). `for x in &array` and `for i in 0..n` work because the iterator code is core's MIR (see 7).
4. **Inherent methods** (`impl Particle { fn step(&mut self) }`, `impl Kind { fn glyph(self) }`): found via
   `Module::impl_defs` -> `Impl::items`, slot names like `Particle::step`. Every module is walked
   (`Crate::modules`), and every `src/**/*.rs` is watched, so multi-file guests work (`mod extra;`).
5. **Statics** are allocated once by the host from `const_eval_static` bytes and keyed by name. Code never embeds
   a static's address: it loads it from a per-static cell that lives forever. That matters because `main`'s
   loop keeps running old code (no on-stack replacement): when a static's layout changes, the host allocates
   new memory, copies every top-level field whose name and layout are unchanged, and swaps the cell at the same
   moment as the fn slots, so even the old `main` frame sees the new memory. (Found the hard way: with an
   embedded address the old `main` passed the old, smaller World to the new `step`.)
6. **Signature and layout changes recompile dependents.** Each fn's recompile key = hash of its MIR text +
   layout keys of all its local types (size, field offsets and field types, recursively through pointers,
   arrays, enum variants) + evaluated values of the consts it uses + the cells of statics it uses + for each
   callee: resolved slot and a signature key (param/return types and layouts). So `bounce(v) -> bounce(v,
   pct)` recompiles `bounce` and `Particle::step`; adding a field to `World` recompiles the five fns that touch
   World even though their MIR text is identical.
7. **Generics and trait calls (stretch, works).** Calls are resolved through `lookup_impl_method` (trait method
   -> impl) and every reachable instance is collected by a worklist and compiled from
   `monomorphized_mir_body`: generic guest fns at each concrete type (`clamp[i64]`, `sq[i32]`), and core's own
   functions (`<i64 as PartialOrd>::lt`, `Range::next`, `slice::Iter::next`, `i64::pow`, `abs`, `min`,
   `f64::max`, `wrapping_add`, ...). About 40 intrinsics are shimmed inline (`offset`, `ptr_metadata`,
   `*_with_overflow`, `ctpop/ctlz/cttz`, `transmute`, `size_of`, `const_eval_select`, `abort`, ...);
   `panic_impl` is a host fn that prints and exits. The sim compiles 108 fns, 99 of them instances from core.
   `probe/` checks abs, min, pow, generic `sq`, `sum(&[T])`, f64 methods, tuples, wrapping math,
   `Option`/`if let` and a `for` loop: `ra-jit probe --once` prints the same 12 values as the native build.

## Per-edit cost and file size (the scaling finding)

ra's incrementality is per file: editing a file re-parses it and re-lowers every function body in it (they
all hang off the file's syntax tree; salsa backdates the unchanged ones, but the lowering work is done). With
300 generated fns in the same `main.rs`, a one-line edit costs ~44 ms of ra time (`guest_fns` 16-18 ms:
signature/generic-param queries re-run for every fn in the file; MIR 18 ms; my own MIR pretty-print for keys
9 ms). Two fixes made it nearly flat for edits outside the big file: (a) multi-file guests, so the 300 fns live
in `extra.rs` and an edit to `main.rs` does not re-lower them; (b) the MIR text hash of a fn whose file did not
change is reused (its key still re-checks consts, callee signatures, layouts and statics, which is how edits in
other files reach it). Result: a sim edit with 300 fns in another file costs 11.3 ms vs 7.3 ms without them (not quite flat: every
update still re-verifies MIR and re-keys all 409 fns, ~10 us each), while an edit inside the 300-fn file costs
~58 ms. cargo grows from 169 ms to 205-282 ms, so the gap is 4-23x depending on where the edit lands.

## How to run (on a dev host, never the Mac)

```
bash vendor.sh                       # copies ra_ap_hir_ty 0.0.358 from the registry, makes Operand::kind pub
rustup component add rust-src --toolchain 1.99.0   # ra needs the sysroot sources to resolve core
cargo build --release
./target/release/ra-jit sim --once        # cold timings, runs `init` then `frame` 3 times
./target/release/ra-jit sim               # watch mode: edit sim/src/*.rs, see timing lines on stderr
./target/release/ra-jit sim --dump-mir    # ra's MIR for every guest fn (RAJIT_VERBOSE=1 lists all instance errors)
python3 live.py                           # six live edits on a running sim -> results/live-sim-sample.txt
bash run_bench.sh                         # the four benchmark configurations -> results/bench-*.txt
python3 bench_sim.py --gen 300 --split --edit sim --edits 20   # one configuration
```

`sim/` (and `guest/`) are normal cargo projects: `src/host.rs` gives `#[no_mangle]` host fns for the native
build; the JIT skips `mod host` and binds the `extern "C"` declarations in `main.rs` to its own Rust fns by
name (`print_i64`, `print_str`, `put_char`, `sleep_ms`). `sim` output under the JIT is
byte-identical to the native debug build (`--once`, checked with `cmp`).

## ra_ap API notes (new in round 2)

No new vendored patches were needed: the only patch is still `pub kind` on `mir::Operand` (`vendor.sh`).
Everything below is public in 0.0.358.

- Layout: `db.layout_of_ty(ty.store(), ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: hir_crate.base() }.store())`
  -> `Arc<LayoutData<RustcFieldIdx, RustcEnumVariantIdx>>` (`size.bytes()`, `align.abi.bytes()`,
  `fields.offset(i).bytes()`, `variants: Variants::{Single, Multiple { tag, tag_encoding, tag_field, variants }}`).
  Matching `tag.primitive()` needs the `ra-ap-rustc_abi = "=0.166.0"` crate; `tag_field.0.into_raw().into_u32()`.
- Field/projection types: `PlaceTy::from_ty(ty).projection_ty(&infcx, &elem, param_env)` with
  `infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis)`
  (`next_solver::infer::DbInternerInferExt`). Projecting a field of an enum without `variant_id` panics
  ("can't project out of Option<i64>"): pass `PlaceTy { ty, variant_id: Some(EnumVariantId) }`.
- Const items are `OperandKind::Constant { konst: Unevaluated(ConstId) }` and pretty-print as `Const(GRAVITY)`:
  the value is not in the MIR text, so a const edit does not change the caller's MIR. Evaluate with
  `db.const_eval(id, args, None)` (bytes in `Allocation::memory`) and hash the value into the key.
- Statics: `OperandKind::Static(StaticId)` is a `&T`; `db.const_eval_static(sid)` gives the initial bytes.
  `hir::Static::from(sid).name(db)` for a stable key. `static mut` needs `#[allow(static_mut_refs)]` in 2024.
- Methods: `Module::impl_defs(db)` -> `Impl::items(db)`; skip trait impls and generic impls as roots.
  `Crate::modules(db)` + `Module::path_to_root(db)` for names; file of a module:
  `module.definition_source_file_id(db).original_file(db).file_id(db)`.
- Trait calls: `db.lookup_impl_method(env, fid, args)` -> `(Either<FunctionId, builtin-derive>, args)`;
  generic instances: `db.monomorphized_mir_body(InferBodyId::DefWithBodyId(fid.into()), args.store(), env.store())`.
  Intrinsics: `hir_def::signatures::FunctionSignature::is_intrinsic(db, fid)`.
- ra MIR quirks: no bounds-check `Assert` for indexing; `CheckedBinaryOp` yields the plain value (no overflow
  tuple, no overflow checks); discriminant temps are typed `i128` (the JIT lowers i128/u128 as 64-bit);
  fn items passed as values (`const_eval_select`'s runtime fn) are locals of `FnDef` type.
- `MirBody::pretty_print` panics on some f64 constants (`display.rs:863`, `try_into().unwrap()`); the JIT
  catches it and hashes the `Debug` form instead.
- `gen` is a reserved keyword in edition 2024: a guest module cannot be called `gen`.

## Limitations (known, not fixed)

- No closures, trait objects/`dyn`, unions, heap (`Box`/`Vec`/`String`), std (only core MIR is reachable;
  `println!` needs std's formatting), unwinding (panics print and exit), real i128/u128, f16/f128, SIMD.
- No on-stack replacement: an edit to `main` swaps its slot but the running loop keeps the old code (the
  thin-main + static design works around it). Statics' initialisers are evaluated once; editing an initializer
  does not reset the running state (only a layout change re-allocates, with field-by-name migration of
  top-level fields; nested changes, e.g. a new field in `Particle`, reset the whole `ps` array).
- Race on layout change: old code still executing an old frame (e.g. mid-`step`) can write the old memory after
  the fields were copied; at most one frame of updates is lost.
- New source files need a restart (VFS loaded once). The watcher polls `src/**/*.rs` every 2 ms.
- The instance worklist is eager: every core fn reachable from any call is compiled, including panic paths;
  failures there are reported as "instance, fatal only if called" and their slot traps with a message.
- Key soundness relies on: own file unchanged + same consts/callee sigs/layouts/statics => same code. A change
  in another file that alters this fn's MIR without touching any of those would be missed (not seen yet).
- ~2.2k lines in one `src/main.rs`; no tests (by design for a PoC; `sim` vs native `cmp` and `probe` are the
  end-to-end checks).

## Next steps

1. Heap and std: shim `alloc`'s global allocator to host malloc/free, then `Box`/`Vec` come from core/alloc
   MIR; formatting (`println!`) needs `fmt::Arguments` with `dyn` (vtables) first.
2. Trait objects and closures: vtables from `ra`'s method resolution; closures via `mir_body_for_closure`.
3. Lazy instance compilation (compile a callee on first call through a stub) instead of the eager worklist:
   cuts cold Cranelift time and the per-edit key work for the ~100 core instances.
4. Per-fn change detection without re-lowering the whole edited file: ask ra for the edited item (syntax
   diff of the file -> `ast_id` of the changed fn) and only re-query that fn plus its dependents.
5. OSR for loops in `main` (or a convention: the host calls `frame()` each tick instead of `main` looping).
6. Upstream `Operand::kind` (pub accessor) so no vendoring is needed; report the `display.rs:863` panic.
