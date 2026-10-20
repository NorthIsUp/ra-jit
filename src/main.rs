//! ra-jit: rust-analyzer MIR -> Cranelift JIT with per-function hot swap.
//!
//! usage: ra-jit <guest-dir> [--once] [--dump-mir]
//!   --once : compile everything, call the guest's `init` once and `frame` 3 times, print cold timings.
//!   default: call `frame` forever on a thread, watch <guest>/src/main.rs,
//!            recompile only functions whose key (MIR text + layouts + consts + callee
//!            signatures + static addresses) changed and swap their table slot.
//!
//! Memory model: scalars that are never borrowed live in Cranelift variables; everything
//! else (structs, tuples, arrays, borrowed scalars) lives in a stack slot sized by
//! rust-analyzer's own layout query. References are real addresses. Guest-to-guest ABI:
//! scalars by value, aggregates by pointer (callee copies), aggregate returns through a
//! hidden first pointer argument (sret). Program state is owned by the host: the guest declares
//! `fn init() -> S` and `fn frame(s: &mut S)` (see `SYSTEMS`); the host allocates S's memory
//! from the layout, runs `init` into it once, and passes it to `frame` every call. It survives
//! swaps, and is re-initialised (with a note) only when S's layout changes. Guest `static`s
//! are also supported: allocated once by the host, keyed by name, and never move across swaps.

mod demo;
mod record;
mod highlight;
mod theme;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use anyhow::{Result, anyhow, bail};
use cranelift_codegen::ir::{
    AbiParam, Block, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind, TrapCode, Type,
    Value, condcodes::FloatCC, condcodes::IntCC, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, default_libcall_names};

use ra_ap_base_db::SourceDatabase;
use ra_ap_hir as hir;
use either::Either;
use ra_ap_hir_def::{FunctionId, ItemContainerId, Lookup, StaticId};
use ra_ap_hir_ty::db::{GeneralConstId, HirDatabase};
use ra_ap_hir_ty::layout::{Layout, RustcEnumVariantIdx, TagEncoding, Variants};
use ra_ap_hir_def::{EnumVariantId, VariantId};
use ra_ap_rustc_abi::Primitive;
use ra_ap_hir_ty::mir::{
    AggregateKind, BasicBlockId, BinOp, CastKind, FieldIndex, LocalId, MirBody, Operand, OperandKind, PlaceTy,
    ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind, UnOp,
};
use ra_ap_hir_ty::next_solver::infer::{DbInternerInferExt, InferCtxt};
use ra_ap_hir_ty::next_solver::{
    Allocation, Const, ConstKind, DbInterner, GenericArgs, ParamEnv, Ty, TyKind, TypingMode,
};
use ra_ap_hir_ty::{InferBodyId, MemoryMap, ParamEnvAndCrate, PointerCast};
use ra_ap_ide_db::RootDatabase;
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};
use ra_ap_project_model::{CargoConfig, RustLibSource};
use ra_ap_rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
use ra_ap_rustc_type_ir::{FloatTy, IntTy, UintTy};

// ---------------------------------------------------------------- host fns

/// Held by the frame loop while it reads (state pointer, code pointer), and by the updater while
/// it publishes a re-initialised state together with the new code, so a frame never gets new
/// code with old-layout memory or the reverse.
static STATE_PAIR: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Host-owned guest states: (init fn, frame fn). Each is one resource `S` with
/// `fn <init>() -> S` and `fn <frame>(s: &mut S)`. More pairs could be added here.
const SYSTEMS: &[(&str, &str)] = &[("init", "frame")];

/// Host memory for one guest state. `cell` holds its address and lives forever.
struct StateSlot {
    key: String,
    cell: &'static AtomicUsize,
}

extern "C" fn host_print_i64(x: i64) {
    println!("{x}");
}
extern "C" fn host_print_str(ptr: *const u8, len: usize) {
    let s = unsafe { std::slice::from_raw_parts(ptr, len) };
    print!("{}", String::from_utf8_lossy(s));
}
extern "C" fn host_put_char(c: u32) {
    print!("{}", char::from_u32(c).unwrap_or('?'));
}
/// Table slots of functions that never compiled point here.
extern "C" fn host_missing() {
    eprintln!("ra-jit: called a guest fn that has no compiled code (see the compile errors above)");
    std::process::exit(101);
}
/// core's `panic_fmt` ends in `extern "Rust" fn panic_impl`, which std normally provides.
extern "C" fn host_panic_impl() {
    eprintln!("ra-jit: guest panicked");
    std::process::exit(101);
}
extern "C" fn host_memmove(dst: *mut u8, src: *const u8, n: usize) {
    unsafe { std::ptr::copy(src, dst, n) }
}

const HOST_FNS: &[(&str, *const u8)] = &[
    ("print_i64", host_print_i64 as *const u8),
    ("print_str", host_print_str as *const u8),
    ("put_char", host_put_char as *const u8),
    ("sleep_ms", demo::host_sleep_ms as *const u8),
    ("set_cell", demo::host_set_cell as *const u8),
    ("present", demo::host_present as *const u8),
    ("panic_impl", host_panic_impl as *const u8),
];

// ---------------------------------------------------------------- front end

struct Front {
    db: RootDatabase,
    /// Every .rs file under <guest>/src known to the VFS at load time (new files need a reload).
    files: Vec<SrcFile>,
}

struct SrcFile {
    path: PathBuf,
    file_id: ra_ap_vfs::FileId,
    stamp: Option<(SystemTime, u64)>,
    text: String,
}

/// Key on (mtime, len): an in-place write can be read half-done with the final mtime unchanged.
fn stamp(p: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(p).and_then(|m| Ok((m.modified()?, m.len()))).ok()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// One guest function (free fn or inherent method) as seen in a given db revision.
struct GuestFn {
    name: String,
    fid: FunctionId,
    file: ra_ap_vfs::FileId,
    def: InferBodyId<'static>,
}

impl Front {
    fn load(guest: &Path) -> Result<Front> {
        let cargo_config = CargoConfig {
            sysroot: Some(RustLibSource::Discover),
            all_targets: false,
            ..Default::default()
        };
        let load_config = LoadCargoConfig {
            load_out_dirs_from_check: false,
            with_proc_macro_server: ProcMacroServerChoice::None,
            prefill_caches: false,
            num_worker_threads: 1,
            proc_macro_processes: 1,
        };
        let (db, vfs, _pm) = load_workspace_at(guest, &cargo_config, &load_config, &|_| {})?;
        let mut paths = vec![];
        rs_files(&std::fs::canonicalize(guest.join("src"))?, &mut paths);
        let mut files = vec![];
        for path in paths {
            let abs = ra_ap_paths::AbsPathBuf::assert_utf8(path.clone());
            let Some((file_id, _)) = vfs.file_id(&ra_ap_vfs::VfsPath::from(abs)) else { continue };
            let text = std::fs::read_to_string(&path)?;
            files.push(SrcFile { stamp: stamp(&path), path, file_id, text });
        }
        if files.is_empty() {
            bail!("no guest source files in the vfs");
        }
        Ok(Front { db, files })
    }

    /// Push every changed file's text into the db (one salsa revision per file); returns how many.
    fn poll(&mut self) -> (HashSet<ra_ap_vfs::FileId>, Option<SystemTime>) {
        let mut n = HashSet::new();
        let mut first_mtime = None;
        for i in 0..self.files.len() {
            let st = stamp(&self.files[i].path);
            if st.is_none() || st == self.files[i].stamp {
                continue;
            }
            self.files[i].stamp = st;
            let Ok(text) = std::fs::read_to_string(&self.files[i].path) else { continue };
            if text == self.files[i].text {
                continue;
            }
            self.db.set_file_text(self.files[i].file_id, &text);
            self.files[i].text = text;
            first_mtime = first_mtime.or(st.map(|s| s.0));
            n.insert(self.files[i].file_id);
        }
        (n, first_mtime)
    }

    /// Non-generic free fns and inherent methods with a body, in every module of the guest crate.
    fn guest_fns(&self) -> Result<(Vec<GuestFn>, hir::Crate)> {
        let db: &dyn HirDatabase = &self.db;
        let krate = hir::Crate::all(db)
            .into_iter()
            .find(|k| k.origin(db).is_local())
            .ok_or_else(|| anyhow!("no local crate"))?;
        let mut out = vec![];
        let mut push = |f: hir::Function, name: String, file: ra_ap_vfs::FileId| {
            if !f.has_body(db) || !hir::GenericDef::Function(f).type_or_const_params(db).is_empty() {
                return;
            }
            let Ok(fid) = FunctionId::try_from(f) else { return };
            out.push(GuestFn {
                name,
                fid,
                file,
                def: InferBodyId::DefWithBodyId(ra_ap_hir_def::DefWithBodyId::FunctionId(fid)),
            });
        };
        for module in krate.modules(db) {
            // `mod host` holds the native build's host shims; the JIT binds host fns itself.
            if module.name(db).is_some_and(|n| n.as_str() == "host") {
                continue;
            }
            let file = module.definition_source_file_id(db).original_file(db).file_id(db);
            // `main`, `Particle::step`, `extra::gen_17`, `extra::Foo::bar`
            let mut prefix = String::new();
            for m in module.path_to_root(db).into_iter().rev() {
                if let Some(n) = m.name(db) {
                    prefix.push_str(n.as_str());
                    prefix.push_str("::");
                }
            }
            for decl in module.declarations(db) {
                if let hir::ModuleDef::Function(f) = decl {
                    push(f, format!("{prefix}{}", f.name(db).as_str()), file);
                }
            }
            for imp in module.impl_defs(db) {
                if imp.trait_(db).is_some() || !hir::GenericDef::Impl(imp).type_or_const_params(db).is_empty() {
                    continue;
                }
                let Some(adt) = imp.self_ty(db).as_adt() else { continue };
                let ty_name = adt.name(db).as_str().to_owned();
                for item in imp.items(db) {
                    if let hir::AssocItem::Function(f) = item {
                        push(f, format!("{prefix}{ty_name}::{}", f.name(db).as_str()), file);
                    }
                }
            }
        }
        Ok((out, krate))
    }
}

// ---------------------------------------------------------------- types and layouts

/// How a Rust type is represented in generated code.
#[derive(Clone, Debug, PartialEq)]
enum Repr {
    /// No runtime value: (), !, fn items, zero-sized structs.
    Zst,
    /// One or two Cranelift values (two only for fat pointers: data ptr at +0, len at +8).
    Scalar(Vec<Type>),
    /// Lives in memory; values of this type are passed around as addresses.
    Mem { size: u32, align: u32 },
}

/// Type queries for monomorphic guest code (empty param env).
struct TyCx<'db> {
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    infcx: InferCtxt<'db>,
    reprs: RefCell<HashMap<Ty<'db>, Repr>>,
    keys: RefCell<HashMap<Ty<'db>, String>>,
    resolved: RefCell<HashMap<(FunctionId, GenericArgs<'db>), Result<Target<'db>, String>>>,
}

impl<'db> TyCx<'db> {
    fn new(db: &'db dyn HirDatabase, krate: hir::Crate) -> TyCx<'db> {
        let krate = krate.base();
        let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis);
        TyCx { db, env: ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }, infcx, reprs: Default::default(), keys: Default::default(), resolved: Default::default() }
    }

    fn layout(&self, ty: Ty<'db>) -> Result<impl std::ops::Deref<Target = Layout> + use<'db>> {
        self.db.layout_of_ty(ty.store(), self.env.store()).map_err(|e| anyhow!("layout of {ty:?}: {e:?}"))
    }

    fn repr(&self, ty: Ty<'db>) -> Result<Repr> {
        if let Some(r) = self.reprs.borrow().get(&ty) {
            return Ok(r.clone());
        }
        let r = match ty.kind() {
            TyKind::Bool => Repr::Scalar(vec![types::I8]),
            TyKind::Char => Repr::Scalar(vec![types::I32]),
            TyKind::Int(i) => Repr::Scalar(vec![match i {
                IntTy::I8 => types::I8,
                IntTy::I16 => types::I16,
                IntTy::I32 => types::I32,
                // i128 is truncated to 64 bits: ra types discriminant temps as i128.
                IntTy::I64 | IntTy::Isize | IntTy::I128 => types::I64,
            }]),
            TyKind::Uint(u) => Repr::Scalar(vec![match u {
                UintTy::U8 => types::I8,
                UintTy::U16 => types::I16,
                UintTy::U32 => types::I32,
                UintTy::U64 | UintTy::Usize | UintTy::U128 => types::I64,
            }]),
            TyKind::Float(f) => Repr::Scalar(vec![match f {
                FloatTy::F32 => types::F32,
                FloatTy::F64 => types::F64,
                _ => bail!("f16/f128 unsupported"),
            }]),
            TyKind::Ref(_, inner, _) | TyKind::RawPtr(inner, _) => match inner.kind() {
                TyKind::Str | TyKind::Slice(_) => Repr::Scalar(vec![types::I64, types::I64]),
                TyKind::Dynamic(..) => bail!("trait objects unsupported"),
                _ => Repr::Scalar(vec![types::I64]),
            },
            TyKind::FnDef(..) | TyKind::Never => Repr::Zst,
            TyKind::Tuple(t) if t.is_empty() => Repr::Zst,
            TyKind::Adt(..) | TyKind::Tuple(_) | TyKind::Array(..) => {
                let l = self.layout(ty)?;
                let size = l.size.bytes();
                if size == 0 {
                    Repr::Zst
                } else {
                    Repr::Mem { size: size as u32, align: l.align.abi.bytes() as u32 }
                }
            }
            other => bail!("unsupported type {other:?}"),
        };
        self.reprs.borrow_mut().insert(ty, r.clone());
        Ok(r)
    }

    fn size_of(&self, ty: Ty<'db>) -> Result<u64> {
        Ok(self.layout(ty)?.size.bytes())
    }

    fn field_offset(&self, ty: Ty<'db>, i: u32) -> Result<u64> {
        let l = self.layout(ty)?;
        Ok(l.fields.offset(i as usize).bytes())
    }

    /// Field offset inside an enum variant (Downcast + Field), or of a plain struct/tuple field.
    fn variant_field_offset(&self, ty: Ty<'db>, variant: Option<VariantId>, i: u32) -> Result<u64> {
        let Some(VariantId::EnumVariantId(v)) = variant else { return self.field_offset(ty, i) };
        let l = self.layout(ty)?;
        Ok(match &l.variants {
            Variants::Multiple { variants, .. } => {
                variants[RustcEnumVariantIdx(v.index(self.db))].fields.offset(i as usize).bytes()
            }
            _ => l.fields.offset(i as usize).bytes(),
        })
    }

    fn array_len(&self, ty: Ty<'db>) -> Option<u64> {
        match ty.kind() {
            TyKind::Array(_, c) => ra_ap_hir_ty::consteval::try_const_usize(self.db, c).map(|n| n as u64),
            _ => None,
        }
    }

    fn proj(&self, pt: PlaceTy<'db>, elem: &ProjectionElem<LocalId>) -> PlaceTy<'db> {
        pt.projection_ty(&self.infcx, elem, self.env.param_env)
    }

    /// A key that changes when the type's runtime layout changes: size and field offsets of
    /// the type and, recursively, of everything reachable through fields, arrays and pointers
    /// (a fn taking `&mut World` must be recompiled when World's fields move).
    fn layout_key(&self, ty: Ty<'db>, out: &mut String) {
        if let Some(k) = self.keys.borrow().get(&ty) {
            out.push_str(k);
            return;
        }
        let mut k = String::new();
        let mut seen = HashSet::new();
        self.layout_key_rec(ty, &mut k, &mut seen);
        out.push_str(&k);
        self.keys.borrow_mut().insert(ty, k);
    }

    fn layout_key_rec(&self, ty: Ty<'db>, out: &mut String, seen: &mut HashSet<Ty<'db>>) {
        let _ = write!(out, "{ty:?}=");
        if !seen.insert(ty) {
            out.push_str("rec;");
            return;
        }
        match self.repr(ty) {
            Ok(Repr::Mem { .. }) => {
                let Ok(l) = self.layout(ty) else { return };
                let _ = write!(out, "{}:{:?}{{", l.size.bytes(), l.fields);
                match ty.kind() {
                    TyKind::Array(elem, _) => self.layout_key_rec(elem, out, seen),
                    TyKind::Adt(def, _) if def.is_enum() => {
                        let _ = write!(out, "{:?}", l.variants);
                        let ra_ap_hir_def::AdtId::EnumId(e) = def.def_id() else { return };
                        for v in hir::Enum::from(e).variants(self.db) {
                            let vid = Some(VariantId::EnumVariantId(v.into()));
                            for i in 0..v.fields(self.db).len() {
                                let fe = ProjectionElem::<LocalId>::Field(FieldIndex(i as u32));
                                let fty = self.proj(PlaceTy { ty, variant_id: vid }, &fe).ty;
                                self.layout_key_rec(fty, out, seen);
                            }
                        }
                    }
                    _ => {
                        for i in 0..l.fields.count() {
                            let fe = ProjectionElem::<LocalId>::Field(FieldIndex(i as u32));
                            let fty = self.proj(PlaceTy::from_ty(ty), &fe).ty;
                            self.layout_key_rec(fty, out, seen);
                        }
                    }
                }
                out.push('}');
            }
            Ok(r) => {
                let _ = write!(out, "{r:?};");
                if let Some(inner) = ty.builtin_deref(true) {
                    self.layout_key_rec(inner, out, seen);
                }
            }
            Err(e) => {
                let _ = write!(out, "err {e};");
            }
        }
    }

    fn eval_const(&self, konst: Const<'db>) -> Result<Allocation<'db>> {
        match konst.kind() {
            ConstKind::Unevaluated(uc) => {
                let r = match uc.def.0 {
                    GeneralConstId::ConstId(c) => self.db.const_eval(c, uc.args, None),
                    GeneralConstId::StaticId(s) => self.db.const_eval_static(s),
                    GeneralConstId::AnonConstId(a) => self.db.anon_const_eval(a, uc.args, None),
                };
                r.map_err(|e| anyhow!("const eval failed: {e:?}"))
            }
            other => bail!("const kind unsupported: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------- back end

/// A guest static. Code never embeds its address: it loads it from `cell`, which lives as long
/// as the process, so even an old, still-running frame (e.g. `main`'s loop) sees a re-allocated
/// static after a layout change.
struct StaticCell {
    cell: &'static AtomicUsize,
    key: String,
    fields: Vec<FieldInfo>,
}

/// One top-level field of a struct static, for migrating state across a layout change.
#[derive(Clone)]
struct FieldInfo {
    name: String,
    offset: u64,
    size: u64,
    key: String,
}

/// A static re-allocated by this update: fields to copy from the old memory at swap time.
struct PendingStatic {
    cell: &'static AtomicUsize,
    new_addr: usize,
    copies: Vec<(u64, u64, u64)>,
}

struct Jit {
    module: JITModule,
    /// Indirection table: one fn pointer per guest function; never moves.
    table: &'static [AtomicUsize],
    slots: HashMap<String, usize>,
    hashes: HashMap<String, u64>,
    generation: u32,
    statics: HashMap<String, StaticCell>,
    pending_statics: Vec<PendingStatic>,
    /// One per `SYSTEMS` entry, once its `init` has run.
    states: Vec<StateSlot>,
    /// Hash of each fn's MIR pretty-print, refreshed only when the fn's file changed.
    text_hashes: HashMap<String, u64>,
}

const TABLE_SLOTS: usize = 8192;

impl Jit {
    fn new() -> Result<Jit> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed")?;
        flags.set("is_pic", "false")?;
        let isa = cranelift_native::builder()
            .map_err(|e| anyhow!(e))?
            .finish(settings::Flags::new(flags))?;
        let jb = JITBuilder::with_isa(isa, default_libcall_names());
        let table: Vec<AtomicUsize> =
            (0..TABLE_SLOTS).map(|_| AtomicUsize::new(host_missing as *const u8 as usize)).collect();
        Ok(Jit {
            module: JITModule::new(jb),
            table: Box::leak(table.into_boxed_slice()),
            slots: HashMap::new(),
            hashes: HashMap::new(),
            generation: 0,
            statics: HashMap::new(),
            pending_statics: vec![],
            states: vec![],
            text_hashes: HashMap::new(),
        })
    }

    fn slot(&mut self, name: &str) -> usize {
        let n = self.slots.len();
        *self.slots.entry(name.to_owned()).or_insert(n)
    }
}

/// Leak `bytes` into 8-byte aligned memory that is never freed.
fn leak_aligned(bytes: &[u8], size: usize) -> usize {
    let words = vec![0u64; size.div_ceil(8).max(1)];
    let p = Box::leak(words.into_boxed_slice()).as_mut_ptr() as *mut u8;
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len().min(size)) };
    p as usize
}

/// Everything a function body needs to know about the rest of the program.
struct CompileEnv {
    /// Non-generic guest fns (any generic args at their call sites are lifetimes only).
    roots: HashSet<FunctionId>,
    /// Table slot per instance key (fn, rendered generic args; "" for roots).
    slots: HashMap<(FunctionId, String), usize>,
    statics: HashMap<StaticId, usize>,
    table_addr: i64,
}

enum Callee<'db> {
    /// Tuple-struct or enum-variant constructor `Some(x)`, `P(1, 2)`.
    Ctor(VariantId),
    Guest(usize),
    Host(*const u8),
    Intrinsic(String, GenericArgs<'db>),
}

fn int_type(bytes: u64) -> Result<Type> {
    Ok(match bytes {
        1 => types::I8,
        2 => types::I16,
        4 => types::I32,
        8 => types::I64,
        n => bail!("{n}-byte integer unsupported"),
    })
}

/// Integer width conversion (sign- or zero-extend, or truncate) to `t`.
fn fit(b: &mut FunctionBuilder, v: Value, t: Type, signed: bool) -> Value {
    let from = b.func.dfg.value_type(v);
    if from == t || !from.is_int() || !t.is_int() {
        v
    } else if t.bits() > from.bits() {
        if signed { b.ins().sextend(t, v) } else { b.ins().uextend(t, v) }
    } else {
        b.ins().ireduce(t, v)
    }
}

enum Storage {
    Vars(Vec<Variable>),
    Slot(cranelift_codegen::ir::StackSlot),
}

/// Where a place lives: a register local, or memory at `base + offset`.
#[derive(Clone, Copy)]
enum Loc {
    Var(LocalId),
    Addr(Value, i32),
}

struct PlaceRes<'db> {
    loc: Loc,
    ty: Ty<'db>,
    /// Length metadata after dereferencing a fat pointer.
    meta: Option<Value>,
}

/// A value: scalars, or the address of an in-memory aggregate.
enum Val {
    S(Vec<Value>),
    M(Value),
}

fn flags() -> MemFlagsData {
    MemFlagsData::trusted()
}

struct Lower<'a, 'db> {
    tcx: &'a TyCx<'db>,
    body: &'a MirBody<'db>,
    storage: HashMap<LocalId, Storage>,
    blocks: HashMap<BasicBlockId, Block>,
    cenv: &'a CompileEnv,
    sret: Option<Value>,
}

impl<'a, 'db> Lower<'a, 'db> {
    fn local_ty(&self, l: LocalId) -> Ty<'db> {
        self.body.locals[l].ty.as_ref()
    }

    fn addr(b: &mut FunctionBuilder, base: Value, off: i32) -> Value {
        if off == 0 { base } else { b.ins().iadd_imm_s(base, off as i64) }
    }

    fn place_ty(&self, p: &StoredPlace) -> PlaceTy<'db> {
        let mut pt = PlaceTy::from_ty(self.local_ty(p.local));
        for e in p.projection.as_slice() {
            pt = self.tcx.proj(pt, e);
        }
        pt
    }

    fn place(&self, b: &mut FunctionBuilder, p: &StoredPlace) -> Result<PlaceRes<'db>> {
        let mut pt = PlaceTy::from_ty(self.local_ty(p.local));
        let mut loc = match self.storage.get(&p.local) {
            Some(Storage::Vars(_)) => Loc::Var(p.local),
            Some(Storage::Slot(s)) => Loc::Addr(b.ins().stack_addr(types::I64, *s, 0), 0),
            None => bail!("local {:?} has unsupported type {:?}", p.local, pt.ty),
        };
        let mut meta = None;
        for elem in p.projection.as_slice() {
            let prev = pt;
            pt = self.tcx.proj(pt, elem);
            match elem {
                ProjectionElem::Deref => {
                    let Val::S(v) = self.read(b, loc, prev.ty)? else { bail!("deref of non-pointer") };
                    let Some(&ptr) = v.first() else { bail!("deref of zero-sized pointer") };
                    loc = Loc::Addr(ptr, 0);
                    meta = v.get(1).copied();
                }
                ProjectionElem::Field(f) => {
                    let Loc::Addr(a, off) = loc else { bail!("field of a register local") };
                    loc = Loc::Addr(a, off + self.tcx.variant_field_offset(prev.ty, prev.variant_id, f.0)? as i32);
                }
                ProjectionElem::Downcast(_) => {} // offsets come from the variant layout at Field
                ProjectionElem::Index(il) => {
                    let Loc::Addr(a, off) = loc else { bail!("index of a register local") };
                    let idx_ty = self.local_ty(*il);
                    let Val::S(iv) = self.read_local(b, *il, idx_ty)? else { bail!("bad index") };
                    let idx = iv[0];
                    // Bounds check (ra MIR has no Assert for indexing): trap on idx >= len.
                    let oob = match (self.tcx.array_len(prev.ty), meta) {
                        (Some(n), _) => b.ins().icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, idx, n as i64),
                        (None, Some(len)) => b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, idx, len),
                        (None, None) => bail!("index into {:?} without a length", prev.ty),
                    };
                    b.ins().trapnz(oob, TrapCode::unwrap_user(5));
                    let esz = self.tcx.size_of(pt.ty)?;
                    let scaled = b.ins().imul_imm_s(idx, esz as i64);
                    let na = b.ins().iadd(a, scaled);
                    loc = Loc::Addr(na, off);
                    meta = None;
                }
                ProjectionElem::ConstantIndex { offset, from_end } => {
                    let Loc::Addr(a, off) = loc else { bail!("index of a register local") };
                    let i = if *from_end {
                        let n = self.tcx.array_len(prev.ty).ok_or_else(|| anyhow!("from_end index of slice"))?;
                        n - offset - 1
                    } else {
                        *offset
                    };
                    let esz = self.tcx.size_of(pt.ty)?;
                    loc = Loc::Addr(a, off + (i * esz) as i32);
                    meta = None;
                }
                other => bail!("projection unsupported: {other:?}"),
            }
        }
        Ok(PlaceRes { loc, ty: pt.ty, meta })
    }

    fn read_local(&self, b: &mut FunctionBuilder, l: LocalId, ty: Ty<'db>) -> Result<Val> {
        let loc = match self.storage.get(&l) {
            Some(Storage::Vars(_)) => Loc::Var(l),
            Some(Storage::Slot(s)) => Loc::Addr(b.ins().stack_addr(types::I64, *s, 0), 0),
            None => bail!("local {l:?} has unsupported type"),
        };
        self.read(b, loc, ty)
    }

    fn read(&self, b: &mut FunctionBuilder, loc: Loc, ty: Ty<'db>) -> Result<Val> {
        Ok(match self.tcx.repr(ty)? {
            Repr::Zst => Val::S(vec![]),
            Repr::Scalar(ts) => match loc {
                Loc::Var(l) => {
                    let Some(Storage::Vars(vs)) = self.storage.get(&l) else { bail!("not a register local") };
                    Val::S(vs.iter().map(|v| b.use_var(*v)).collect())
                }
                Loc::Addr(a, off) => Val::S(
                    ts.iter().enumerate().map(|(i, t)| b.ins().load(*t, flags(), a, off + 8 * i as i32)).collect(),
                ),
            },
            Repr::Mem { .. } => match loc {
                Loc::Addr(a, off) => Val::M(Self::addr(b, a, off)),
                Loc::Var(_) => bail!("aggregate in a register local"),
            },
        })
    }

    fn write(&self, b: &mut FunctionBuilder, loc: Loc, ty: Ty<'db>, v: Val) -> Result<()> {
        match (self.tcx.repr(ty)?, v) {
            (Repr::Zst, _) => {}
            (Repr::Scalar(ts), Val::S(vals)) => {
                if ts.len() != vals.len() {
                    bail!("arity mismatch writing {ty:?}");
                }
                match loc {
                    Loc::Var(l) => {
                        let Some(Storage::Vars(vs)) = self.storage.get(&l) else { bail!("not a register local") };
                        for (var, x) in vs.iter().zip(vals) {
                            b.def_var(*var, x);
                        }
                    }
                    Loc::Addr(a, off) => {
                        for (i, x) in vals.into_iter().enumerate() {
                            b.ins().store(flags(), x, a, off + 8 * i as i32);
                        }
                    }
                }
            }
            (Repr::Mem { size, align }, Val::M(src)) => {
                let Loc::Addr(a, off) = loc else { bail!("aggregate into a register local") };
                let dst = Self::addr(b, a, off);
                copy_mem(b, dst, src, size, align);
            }
            (r, _) => bail!("value/representation mismatch writing {ty:?} ({r:?})"),
        }
        Ok(())
    }

    fn operand_ty(&self, op: &Operand) -> Result<Ty<'db>> {
        Ok(match &op.kind {
            OperandKind::Copy(p) | OperandKind::Move(p) => self.place_ty(p).ty,
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(_) => bail!("type of a static operand"),
        })
    }

    fn operand(&self, b: &mut FunctionBuilder, op: &Operand) -> Result<Val> {
        match &op.kind {
            OperandKind::Copy(p) | OperandKind::Move(p) => {
                let pr = self.place(b, p)?;
                self.read(b, pr.loc, pr.ty)
            }
            OperandKind::Constant { konst, .. } => {
                let a = self.tcx.eval_const(konst.as_ref())?;
                self.materialize(b, a)
            }
            OperandKind::Allocation { allocation } => self.materialize(b, allocation.as_ref()),
            OperandKind::Static(s) => {
                let cell = self.cenv.statics.get(s).ok_or_else(|| anyhow!("static not allocated"))?;
                let c = b.ins().iconst(types::I64, *cell as i64);
                Ok(Val::S(vec![b.ins().load(types::I64, flags(), c, 0)]))
            }
        }
    }

    fn materialize(&self, b: &mut FunctionBuilder, a: Allocation<'db>) -> Result<Val> {
        let ty = a.ty;
        Ok(match self.tcx.repr(ty)? {
            Repr::Zst => Val::S(vec![]),
            Repr::Scalar(ts) if matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..)) => {
                // &str / &[u8] literal: memory = [ptr(0), len], bytes in the memory map.
                let MemoryMap::Simple(bytes) = &a.memory_map else {
                    bail!("pointer constant with complex memory map unsupported")
                };
                let leaked: &'static [u8] = Box::leak(bytes.clone());
                let mut vals = vec![b.ins().iconst(types::I64, leaked.as_ptr() as i64)];
                if ts.len() == 2 {
                    let len = u64::from_le_bytes(a.memory[8..16].try_into()?);
                    vals.push(b.ins().iconst(types::I64, len as i64));
                }
                Val::S(vals)
            }
            Repr::Scalar(ts) => {
                let [t] = ts[..] else { bail!("multi-value constant") };
                let mut buf = [0u8; 8];
                let n = a.memory.len().min(8);
                buf[..n].copy_from_slice(&a.memory[..n]);
                let bits = u64::from_le_bytes(buf);
                Val::S(vec![if t == types::F64 {
                    b.ins().f64const(f64::from_bits(bits))
                } else if t == types::F32 {
                    b.ins().f32const(f32::from_bits(bits as u32))
                } else {
                    b.ins().iconst(t, bits as i64)
                }])
            }
            Repr::Mem { size, .. } => {
                if !matches!(a.memory_map, MemoryMap::Empty) {
                    bail!("aggregate constant containing pointers unsupported");
                }
                let p = leak_aligned(&a.memory, size as usize);
                Val::M(b.ins().iconst(types::I64, p as i64))
            }
        })
    }

    /// A scalar operand; references to scalars are loaded through (ra compares `&a < &b`).
    fn scalar(&self, b: &mut FunctionBuilder, op: &Operand) -> Result<(Value, Ty<'db>)> {
        let ty = self.operand_ty(op)?;
        let Val::S(v) = self.operand(b, op)? else { bail!("expected a scalar operand") };
        let [x] = v[..] else { bail!("expected a single-value operand, got {ty:?}") };
        if let TyKind::Ref(_, inner, _) = ty.kind() {
            if let Repr::Scalar(ts) = self.tcx.repr(inner)? {
                if ts.len() == 1 {
                    return Ok((b.ins().load(ts[0], flags(), x, 0), inner));
                }
            }
            bail!("binary op on a reference to {inner:?}");
        }
        Ok((x, ty))
    }

    fn assign(&self, b: &mut FunctionBuilder, place: &StoredPlace, rv: &Rvalue) -> Result<()> {
        match rv {
            Rvalue::Aggregate(kind, ops) => {
                let vals = ops.iter().map(|o| self.operand(b, o)).collect::<Result<Vec<_>>>()?;
                let dst = self.place(b, place)?;
                match self.tcx.repr(dst.ty)? {
                    Repr::Zst => return Ok(()),
                    Repr::Mem { .. } => {}
                    Repr::Scalar(_) => bail!("scalar aggregate {:?}", dst.ty),
                }
                let Loc::Addr(a, off) = dst.loc else { bail!("aggregate into a register local") };
                for (i, v) in vals.into_iter().enumerate() {
                    let (foff, fty) = match kind {
                        AggregateKind::Array(elem) => {
                            let e = elem.as_ref();
                            (i as u64 * self.tcx.size_of(e)?, e)
                        }
                        AggregateKind::Adt(VariantId::EnumVariantId(v), _) => {
                            let vid = Some(VariantId::EnumVariantId(*v));
                            let fe = ProjectionElem::<LocalId>::Field(FieldIndex(i as u32));
                            let fty = self.tcx.proj(PlaceTy { ty: dst.ty, variant_id: vid }, &fe).ty;
                            (self.tcx.variant_field_offset(dst.ty, vid, i as u32)?, fty)
                        }
                        AggregateKind::Tuple(_) | AggregateKind::Adt(..) => {
                            let fe = ProjectionElem::<LocalId>::Field(FieldIndex(i as u32));
                            let fty = self.tcx.proj(PlaceTy::from_ty(dst.ty), &fe).ty;
                            (self.tcx.field_offset(dst.ty, i as u32)?, fty)
                        }
                        other => bail!("aggregate kind unsupported: {other:?}"),
                    };
                    self.write(b, Loc::Addr(a, off + foff as i32), fty, v)?;
                }
                if let AggregateKind::Adt(VariantId::EnumVariantId(v), _) = kind {
                    self.write_tag(b, a, off, dst.ty, *v)?;
                }
                Ok(())
            }
            Rvalue::Repeat(op, _) => {
                let v = self.operand(b, op)?;
                let dst = self.place(b, place)?;
                let TyKind::Array(elem, _) = dst.ty.kind() else { bail!("repeat into non-array") };
                let n = self.tcx.array_len(dst.ty).ok_or_else(|| anyhow!("array length unknown"))?;
                let esz = self.tcx.size_of(elem)?;
                let Loc::Addr(a, off) = dst.loc else { bail!("array in a register local") };
                if n <= 64 {
                    for i in 0..n {
                        let vi = match &v {
                            Val::S(x) => Val::S(x.clone()),
                            Val::M(p) => Val::M(*p),
                        };
                        self.write(b, Loc::Addr(a, off + (i * esz) as i32), elem, vi)?;
                    }
                } else {
                    // Runtime loop: for i in 0..n { dst[i] = v }
                    let base = Self::addr(b, a, off);
                    let head = b.create_block();
                    let body = b.create_block();
                    let exit = b.create_block();
                    b.append_block_param(head, types::I64);
                    let zero = b.ins().iconst(types::I64, 0);
                    b.ins().jump(head, &[zero.into()]);
                    b.switch_to_block(head);
                    let i = b.block_params(head)[0];
                    let done = b.ins().icmp_imm_s(IntCC::Equal, i, n as i64);
                    b.ins().brif(done, exit, &[], body, &[]);
                    b.switch_to_block(body);
                    let scaled = b.ins().imul_imm_s(i, esz as i64);
                    let ea = b.ins().iadd(base, scaled);
                    let vi = match &v {
                        Val::S(x) => Val::S(x.clone()),
                        Val::M(p) => Val::M(*p),
                    };
                    self.write(b, Loc::Addr(ea, 0), elem, vi)?;
                    let next = b.ins().iadd_imm_s(i, 1);
                    b.ins().jump(head, &[next.into()]);
                    b.switch_to_block(exit);
                }
                Ok(())
            }
            _ => {
                let dst_ty = self.place_ty(place).ty;
                let v = self.rvalue(b, rv, dst_ty)?;
                let dst = self.place(b, place)?;
                self.write(b, dst.loc, dst.ty, v)
            }
        }
    }

    fn rvalue(&self, b: &mut FunctionBuilder, rv: &Rvalue, dest_ty: Ty<'db>) -> Result<Val> {
        Ok(match rv {
            Rvalue::Use(op) => self.operand(b, op)?,
            Rvalue::Ref(_, p) => {
                let pr = self.place(b, p)?;
                let Loc::Addr(a, off) = pr.loc else { bail!("borrow of a register local {:?}", p.local) };
                let mut v = vec![Self::addr(b, a, off)];
                v.extend(pr.meta);
                Val::S(v)
            }
            Rvalue::CopyForDeref(p) => {
                let pr = self.place(b, p)?;
                self.read(b, pr.loc, pr.ty)?
            }
            Rvalue::Discriminant(p) => {
                let pr = self.place(b, p)?;
                Val::S(vec![self.discriminant(b, &pr, dest_ty)?])
            }
            Rvalue::Len(p) => {
                let pr = self.place(b, p)?;
                match (self.tcx.array_len(pr.ty), pr.meta) {
                    (Some(n), _) => Val::S(vec![b.ins().iconst(types::I64, n as i64)]),
                    (None, Some(m)) => Val::S(vec![m]),
                    _ => bail!("len of {:?}", pr.ty),
                }
            }
            Rvalue::CheckedBinaryOp(op, l, r) => {
                let (x, lt) = self.scalar(b, l)?;
                let (y, _) = self.scalar(b, r)?;
                if let TyKind::Float(_) = lt.kind() {
                    let cc = match op {
                        BinOp::Eq => Some(FloatCC::Equal),
                        BinOp::Ne => Some(FloatCC::NotEqual),
                        BinOp::Lt => Some(FloatCC::LessThan),
                        BinOp::Le => Some(FloatCC::LessThanOrEqual),
                        BinOp::Gt => Some(FloatCC::GreaterThan),
                        BinOp::Ge => Some(FloatCC::GreaterThanOrEqual),
                        _ => None,
                    };
                    let i = b.ins();
                    return Ok(Val::S(vec![match (op, cc) {
                        (_, Some(cc)) => i.fcmp(cc, x, y),
                        (BinOp::Add, _) => i.fadd(x, y),
                        (BinOp::Sub, _) => i.fsub(x, y),
                        (BinOp::Mul, _) => i.fmul(x, y),
                        (BinOp::Div, _) => i.fdiv(x, y),
                        (other, _) => bail!("float op {other:?} unsupported"),
                    }]));
                }
                let i = b.ins();
                let s = matches!(lt.kind(), TyKind::Int(_));
                let cmp = |cc_s: IntCC, cc_u: IntCC| if s { cc_s } else { cc_u };
                Val::S(vec![match op {
                    BinOp::Add => i.iadd(x, y),
                    BinOp::Sub => i.isub(x, y),
                    BinOp::Mul => i.imul(x, y),
                    BinOp::Div => if s { i.sdiv(x, y) } else { i.udiv(x, y) },
                    BinOp::Rem => if s { i.srem(x, y) } else { i.urem(x, y) },
                    BinOp::BitXor => i.bxor(x, y),
                    BinOp::BitAnd => i.band(x, y),
                    BinOp::BitOr => i.bor(x, y),
                    BinOp::Shl => i.ishl(x, y),
                    BinOp::Shr => if s { i.sshr(x, y) } else { i.ushr(x, y) },
                    BinOp::Eq => i.icmp(IntCC::Equal, x, y),
                    BinOp::Ne => i.icmp(IntCC::NotEqual, x, y),
                    BinOp::Lt => i.icmp(cmp(IntCC::SignedLessThan, IntCC::UnsignedLessThan), x, y),
                    BinOp::Le => i.icmp(cmp(IntCC::SignedLessThanOrEqual, IntCC::UnsignedLessThanOrEqual), x, y),
                    BinOp::Gt => i.icmp(cmp(IntCC::SignedGreaterThan, IntCC::UnsignedGreaterThan), x, y),
                    BinOp::Ge => i.icmp(cmp(IntCC::SignedGreaterThanOrEqual, IntCC::UnsignedGreaterThanOrEqual), x, y),
                    BinOp::Offset => bail!("pointer offset unsupported"),
                }])
            }
            Rvalue::UnaryOp(op, x) => {
                let (v, t) = self.scalar(b, x)?;
                Val::S(vec![match op {
                    UnOp::Not if t.is_bool() => b.ins().bxor_imm_s(v, 1),
                    UnOp::Not => b.ins().bnot(v),
                    UnOp::Neg if matches!(t.kind(), TyKind::Float(_)) => b.ins().fneg(v),
                    UnOp::Neg => b.ins().ineg(v),
                }])
            }
            Rvalue::Cast(kind, x, _) => {
                let from_t = self.operand_ty(x)?;
                let Repr::Scalar(to_ts) = self.tcx.repr(dest_ty)? else { bail!("cast to non-scalar") };
                match kind {
                    CastKind::IntToInt | CastKind::FloatToInt | CastKind::IntToFloat | CastKind::FloatToFloat => {
                        let (v, _) = self.scalar(b, x)?;
                        let to = to_ts[0];
                        let from = b.func.dfg.value_type(v);
                        let signed = matches!(from_t.kind(), TyKind::Int(_));
                        let to_signed = matches!(dest_ty.kind(), TyKind::Int(_));
                        Val::S(vec![match kind {
                            CastKind::IntToInt => {
                                if to.bits() > from.bits() {
                                    if signed { b.ins().sextend(to, v) } else { b.ins().uextend(to, v) }
                                } else if to.bits() < from.bits() {
                                    b.ins().ireduce(to, v)
                                } else {
                                    v
                                }
                            }
                            CastKind::IntToFloat => {
                                if signed { b.ins().fcvt_from_sint(to, v) } else { b.ins().fcvt_from_uint(to, v) }
                            }
                            CastKind::FloatToInt => {
                                if to_signed { b.ins().fcvt_to_sint_sat(to, v) } else { b.ins().fcvt_to_uint_sat(to, v) }
                            }
                            _ => {
                                if to.bits() > from.bits() {
                                    b.ins().fpromote(to, v)
                                } else if to.bits() < from.bits() {
                                    b.ins().fdemote(to, v)
                                } else {
                                    v
                                }
                            }
                        }])
                    }
                    CastKind::PointerCoercion(PointerCast::Unsize) => {
                        // &[T; N] -> &[T]
                        let Val::S(v) = self.operand(b, x)? else { bail!("unsize of non-pointer") };
                        let inner = from_t.builtin_deref(true).ok_or_else(|| anyhow!("unsize of non-pointer"))?;
                        let n = self.tcx.array_len(inner).ok_or_else(|| anyhow!("unsize of non-array {inner:?}"))?;
                        Val::S(vec![v[0], b.ins().iconst(types::I64, n as i64)])
                    }
                    CastKind::PtrToPtr | CastKind::PointerCoercion(PointerCast::MutToConstPointer) => {
                        let Val::S(mut v) = self.operand(b, x)? else { bail!("ptr cast of non-pointer") };
                        v.truncate(to_ts.len());
                        Val::S(v)
                    }
                    other => bail!("cast unsupported: {other:?}"),
                }
            }
            other => bail!("rvalue unsupported: {other:?}"),
        })
    }

    /// Write the fields (and the enum tag) of a struct or enum variant value into `dst`.
    fn write_fields(&self, b: &mut FunctionBuilder, dst: &PlaceRes<'db>, v: VariantId, vals: Vec<Val>) -> Result<()> {
        if self.tcx.repr(dst.ty)? == Repr::Zst {
            return Ok(());
        }
        let Loc::Addr(a, off) = dst.loc else { bail!("aggregate into a register local") };
        let vid = match v {
            VariantId::EnumVariantId(_) => Some(v),
            _ => None,
        };
        for (i, val) in vals.into_iter().enumerate() {
            let fe = ProjectionElem::<LocalId>::Field(FieldIndex(i as u32));
            let fty = self.tcx.proj(PlaceTy { ty: dst.ty, variant_id: vid }, &fe).ty;
            let foff = self.tcx.variant_field_offset(dst.ty, vid, i as u32)?;
            self.write(b, Loc::Addr(a, off + foff as i32), fty, val)?;
        }
        if let VariantId::EnumVariantId(e) = v {
            self.write_tag(b, a, off, dst.ty, e)?;
        }
        Ok(())
    }

    fn slot_ptr(&self, b: &mut FunctionBuilder, slot: usize) -> Value {
        let addr = b.ins().iconst(types::I64, self.cenv.table_addr + 8 * slot as i64);
        b.ins().load(types::I64, flags(), addr, 0)
    }

    /// Guest ABI call: [sret] ++ args (scalars by value, aggregates by address).
    fn emit_call(&self, b: &mut FunctionBuilder, fptr: Value, args: Vec<Val>, dst: &PlaceRes<'db>) -> Result<()> {
        let mut argv = vec![];
        let mut sig = Signature::new(b.func.signature.call_conv);
        let ret_repr = self.tcx.repr(dst.ty)?;
        if let Repr::Mem { .. } = ret_repr {
            let Loc::Addr(a, off) = dst.loc else { bail!("aggregate call result in register") };
            argv.push(Self::addr(b, a, off));
            sig.params.push(AbiParam::new(types::I64));
        }
        for v in args {
            match v {
                Val::S(vs) => {
                    for v in vs {
                        sig.params.push(AbiParam::new(b.func.dfg.value_type(v)));
                        argv.push(v);
                    }
                }
                Val::M(p) => {
                    sig.params.push(AbiParam::new(types::I64));
                    argv.push(p);
                }
            }
        }
        if let Repr::Scalar(ts) = &ret_repr {
            sig.returns.extend(ts.iter().map(|t| AbiParam::new(*t)));
        }
        let sref = b.import_signature(sig);
        let inst = b.ins().call_indirect(sref, fptr, &argv);
        let rets = b.inst_results(inst).to_vec();
        if let Repr::Scalar(_) = ret_repr {
            self.write(b, dst.loc, dst.ty, Val::S(rets))?;
        }
        Ok(())
    }

    /// The tag of an enum layout: (int type, signed, byte offset).
    fn tag_of(&self, l: &Layout) -> Result<Option<(Type, bool, i32)>> {
        let Variants::Multiple { tag, tag_field, .. } = &l.variants else { return Ok(None) };
        let (t, signed) = match tag.primitive() {
            Primitive::Int(i, s) => (int_type(i.size().bytes())?, s),
            Primitive::Pointer(_) => (types::I64, false),
            Primitive::Float(_) => bail!("float enum tag"),
        };
        let fi = tag_field.0.into_raw().into_u32() as usize;
        Ok(Some((t, signed, l.fields.offset(fi).bytes() as i32)))
    }

    /// Discriminant value of variant `idx` of the enum `ty`.
    fn discr_of(&self, ty: Ty<'db>, idx: usize) -> Result<i128> {
        let TyKind::Adt(def, _) = ty.kind() else { bail!("discriminant of non-enum {ty:?}") };
        let ra_ap_hir_def::AdtId::EnumId(e) = def.def_id() else { bail!("discriminant of non-enum {ty:?}") };
        let v = hir::Enum::from(e).variants(self.tcx.db)[idx];
        v.eval(self.tcx.db).map_err(|e| anyhow!("discriminant: {e:?}"))
    }

    fn write_tag(&self, b: &mut FunctionBuilder, a: Value, off: i32, ty: Ty<'db>, v: EnumVariantId) -> Result<()> {
        let l = self.tcx.layout(ty)?;
        let Some((t, _, toff)) = self.tag_of(&l)? else { return Ok(()) };
        let idx = v.index(self.tcx.db);
        let Variants::Multiple { tag_encoding, .. } = &l.variants else { return Ok(()) };
        let val: Option<u128> = match tag_encoding {
            TagEncoding::Direct => Some(self.discr_of(ty, idx)? as u128),
            TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                if idx == untagged_variant.0 {
                    None
                } else {
                    Some(((idx - niche_variants.start().0) as u128).wrapping_add(*niche_start))
                }
            }
        };
        if let Some(val) = val {
            let c = b.ins().iconst(t, val as u64 as i64);
            b.ins().store(flags(), c, a, off + toff);
        }
        Ok(())
    }

    fn discriminant(&self, b: &mut FunctionBuilder, pr: &PlaceRes<'db>, dest_ty: Ty<'db>) -> Result<Value> {
        let Repr::Scalar(dts) = self.tcx.repr(dest_ty)? else { bail!("discriminant into non-scalar") };
        let dt = dts[0];
        let l = self.tcx.layout(pr.ty)?;
        match &l.variants {
            Variants::Single { index } => {
                let d = self.discr_of(pr.ty, index.0)?;
                Ok(b.ins().iconst(dt, d as i64))
            }
            Variants::Empty => Ok(b.ins().iconst(dt, 0)),
            Variants::Multiple { tag_encoding, variants, .. } => {
                let Loc::Addr(a, off) = pr.loc else { bail!("enum in a register local") };
                let (t, signed, toff) = self.tag_of(&l)?.expect("multiple variants have a tag");
                let tag = b.ins().load(t, flags(), a, off + toff);
                match tag_encoding {
                    TagEncoding::Direct => Ok(fit(b, tag, dt, signed)),
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        for i in 0..variants.len() {
                            if self.discr_of(pr.ty, i)? != i as i128 {
                                bail!("niche-encoded enum with explicit discriminants unsupported");
                            }
                        }
                        let (start, end) = (niche_variants.start().0 as i64, niche_variants.end().0 as i64);
                        let rel = b.ins().iadd_imm_s(tag, (*niche_start as i64).wrapping_neg());
                        let rel = fit(b, rel, types::I64, false);
                        let in_range = b.ins().icmp_imm_s(IntCC::UnsignedLessThanOrEqual, rel, end - start);
                        let tagged = b.ins().iadd_imm_s(rel, start);
                        let untagged = b.ins().iconst(types::I64, untagged_variant.0 as i64);
                        let idx = b.ins().select(in_range, tagged, untagged);
                        Ok(fit(b, idx, dt, false))
                    }
                }
            }
        }
    }

    /// Inline implementations of the compiler intrinsics core's MIR calls most often.
    /// Returns true if the call diverges.
    fn intrinsic(
        &self,
        b: &mut FunctionBuilder,
        name: &str,
        gargs: GenericArgs<'db>,
        args: &[Operand],
        dst: &PlaceRes<'db>,
    ) -> Result<bool> {
        let vals = args.iter().map(|a| self.operand(b, a)).collect::<Result<Vec<_>>>()?;
        let tys = args.iter().map(|a| self.operand_ty(a)).collect::<Result<Vec<_>>>()?;
        let sc = |i: usize| -> Result<Value> {
            match vals.get(i) {
                Some(Val::S(v)) if v.len() == 1 => Ok(v[0]),
                _ => bail!("intrinsic {name}: argument {i} is not a scalar"),
            }
        };
        let signed = tys.first().is_some_and(|t| matches!(t.kind(), TyKind::Int(_)));
        let dst_t = match self.tcx.repr(dst.ty)? {
            Repr::Scalar(ts) if ts.len() == 1 => Some(ts[0]),
            _ => None,
        };
        let v = match name {
            "wrapping_add" | "unchecked_add" => b.ins().iadd(sc(0)?, sc(1)?),
            "wrapping_sub" | "unchecked_sub" => b.ins().isub(sc(0)?, sc(1)?),
            "wrapping_mul" | "unchecked_mul" => b.ins().imul(sc(0)?, sc(1)?),
            "unchecked_div" | "exact_div" => {
                if signed { b.ins().sdiv(sc(0)?, sc(1)?) } else { b.ins().udiv(sc(0)?, sc(1)?) }
            }
            "unchecked_rem" => {
                if signed { b.ins().srem(sc(0)?, sc(1)?) } else { b.ins().urem(sc(0)?, sc(1)?) }
            }
            "unchecked_shl" => b.ins().ishl(sc(0)?, sc(1)?),
            "unchecked_shr" => {
                if signed { b.ins().sshr(sc(0)?, sc(1)?) } else { b.ins().ushr(sc(0)?, sc(1)?) }
            }
            "rotate_left" => b.ins().rotl(sc(0)?, sc(1)?),
            "rotate_right" => b.ins().rotr(sc(0)?, sc(1)?),
            "add_with_overflow" | "sub_with_overflow" | "mul_with_overflow" => {
                let (x, y) = (sc(0)?, sc(1)?);
                let i = b.ins();
                let (r, of) = match (name, signed) {
                    ("add_with_overflow", true) => i.sadd_overflow(x, y),
                    ("add_with_overflow", false) => i.uadd_overflow(x, y),
                    ("sub_with_overflow", true) => i.ssub_overflow(x, y),
                    ("sub_with_overflow", false) => i.usub_overflow(x, y),
                    (_, true) => i.smul_overflow(x, y),
                    (_, false) => i.umul_overflow(x, y),
                };
                let Loc::Addr(a, off) = dst.loc else { bail!("tuple result in a register") };
                let o0 = self.tcx.field_offset(dst.ty, 0)? as i32;
                let o1 = self.tcx.field_offset(dst.ty, 1)? as i32;
                b.ins().store(flags(), r, a, off + o0);
                b.ins().store(flags(), of, a, off + o1);
                return Ok(false);
            }
            "ctpop" => b.ins().popcnt(sc(0)?),
            "ctlz" | "ctlz_nonzero" => b.ins().clz(sc(0)?),
            "cttz" | "cttz_nonzero" => b.ins().ctz(sc(0)?),
            "bswap" => {
                let x = sc(0)?;
                if b.func.dfg.value_type(x) == types::I8 { x } else { b.ins().bswap(x) }
            }
            "bitreverse" => b.ins().bitrev(sc(0)?),
            "ptr_metadata" => match &vals[0] {
                Val::S(v) if v.len() == 2 => v[1],
                _ => return Ok(false), // thin pointer: metadata is ()
            },
            "is_val_statically_known" | "ub_checks" => b.ins().iconst(types::I8, 0),
            "caller_location" => b.ins().iconst(types::I64, 0),
            "offset" | "arith_offset" => {
                // ptr + count * size_of::<pointee>()
                let pointee = tys[0].builtin_deref(true).ok_or_else(|| anyhow!("offset of non-pointer"))?;
                let size = self.tcx.size_of(pointee)? as i64;
                let n = fit(b, sc(1)?, types::I64, true);
                let bytes = b.ins().imul_imm_s(n, size);
                b.ins().iadd(sc(0)?, bytes)
            }
            "ptr_offset_from" | "ptr_offset_from_unsigned" => {
                let pointee = tys[0].builtin_deref(true).ok_or_else(|| anyhow!("offset_from of non-pointer"))?;
                let size = self.tcx.size_of(pointee)?.max(1) as i64;
                let d = b.ins().isub(sc(0)?, sc(1)?);
                b.ins().sdiv_imm_s(d, size)
            }
            "const_eval_select" => {
                // const_eval_select(args_tuple, const_fn, rt_fn): call rt_fn(args...)
                let TyKind::FnDef(def, fargs) = tys[2].kind() else { bail!("const_eval_select: rt fn is not a fn item") };
                let ra_ap_hir_def::CallableDefId::FunctionId(fid) = def.0 else { bail!("const_eval_select: not a fn") };
                let Target::Inst(f, a) = resolve(self.tcx, fid, fargs)? else { bail!("const_eval_select: rt fn not compilable") };
                let k = inst_key(&self.cenv.roots, f, a);
                let slot = *self.cenv.slots.get(&k).ok_or_else(|| anyhow!("const_eval_select target not collected"))?;
                let mut call_args = vec![];
                if let Val::M(tup) = &vals[0] {
                    let TyKind::Tuple(fts) = tys[0].kind() else { bail!("const_eval_select: args not a tuple") };
                    for (i, ft) in fts.iter().enumerate() {
                        let o = self.tcx.field_offset(tys[0], i as u32)? as i32;
                        call_args.push(self.read(b, Loc::Addr(*tup, o), ft)?);
                    }
                } else if let Val::S(v) = &vals[0] {
                    // a tuple of one scalar field can be a scalar-repr'd... only () is expected here
                    if !v.is_empty() {
                        bail!("const_eval_select: scalar args tuple");
                    }
                }
                let fptr = self.slot_ptr(b, slot);
                self.emit_call(b, fptr, call_args, dst)?;
                return Ok(false);
            }
            "size_of" | "align_of" | "min_align_of" | "pref_align_of" => {
                let t = gargs.type_at(0);
                let l = self.tcx.layout(t)?;
                let n = if name == "size_of" { l.size.bytes() } else { l.align.abi.bytes() };
                b.ins().iconst(types::I64, n as i64)
            }
            "abort" | "unreachable" => {
                b.ins().trap(TrapCode::unwrap_user(6));
                return Ok(true);
            }
            "assume" | "cold_path" => return Ok(false),
            "likely" | "unlikely" | "black_box" => {
                let v = match &vals[0] {
                    Val::S(x) => Val::S(x.clone()),
                    Val::M(p) => Val::M(*p),
                };
                self.write(b, dst.loc, dst.ty, v)?;
                return Ok(false);
            }
            "transmute" | "transmute_unchecked" => {
                // Reinterpret through memory: spill as the source type, reload as the target type.
                let size = self.tcx.size_of(dst.ty)?.max(self.tcx.size_of(tys[0])?).max(16) as u32;
                let slot = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 4));
                let tmp = b.ins().stack_addr(types::I64, slot, 0);
                let src = match &vals[0] {
                    Val::S(x) => Val::S(x.clone()),
                    Val::M(p) => Val::M(*p),
                };
                self.write(b, Loc::Addr(tmp, 0), tys[0], src)?;
                let v = self.read(b, Loc::Addr(tmp, 0), dst.ty)?;
                self.write(b, dst.loc, dst.ty, v)?;
                return Ok(false);
            }
            other => bail!("intrinsic `{other}` has no shim"),
        };
        let dt = dst_t.ok_or_else(|| anyhow!("intrinsic {name}: non-scalar result"))?;
        let v = fit(b, v, dt, false);
        self.write(b, dst.loc, dst.ty, Val::S(vec![v]))?;
        Ok(false)
    }

    fn callee(&self, func: &Operand) -> Result<Callee<'db>> {
        let ty = self.operand_ty(func)?;
        let TyKind::FnDef(def, args) = ty.kind() else { bail!("indirect call unsupported") };
        let fid = match def.0 {
            ra_ap_hir_def::CallableDefId::FunctionId(f) => f,
            ra_ap_hir_def::CallableDefId::EnumVariantId(v) => return Ok(Callee::Ctor(VariantId::EnumVariantId(v))),
            ra_ap_hir_def::CallableDefId::StructId(st) => return Ok(Callee::Ctor(VariantId::StructId(st))),
        };
        match resolve(self.tcx, fid, args)? {
            Target::Host(p) => Ok(Callee::Host(p)),
            Target::Intrinsic(n, a) => Ok(Callee::Intrinsic(n, a)),
            Target::Inst(f, a) => {
                let k = inst_key(&self.cenv.roots, f, a);
                let slot = self.cenv.slots.get(&k).ok_or_else(|| anyhow!("callee instance {k:?} was not collected"))?;
                Ok(Callee::Guest(*slot))
            }
        }
    }
}

/// What a call resolves to: a host fn, or a function instance whose MIR we compile
/// (a guest fn, a monomorphized generic, or a sysroot fn such as `<i64 as PartialOrd>::lt`).
#[derive(Clone)]
enum Target<'db> {
    Host(*const u8),
    Intrinsic(String, GenericArgs<'db>),
    Inst(FunctionId, GenericArgs<'db>),
}

/// Compiler intrinsics with an inline shim in `Lower::intrinsic`.
const INTRINSICS: &[&str] = &[
    "wrapping_add", "wrapping_sub", "wrapping_mul", "unchecked_add", "unchecked_sub", "unchecked_mul",
    "unchecked_div", "exact_div", "unchecked_rem", "unchecked_shl", "unchecked_shr", "rotate_left",
    "rotate_right", "add_with_overflow", "sub_with_overflow", "mul_with_overflow", "ctpop", "ctlz",
    "ctlz_nonzero", "cttz", "cttz_nonzero", "bswap", "bitreverse", "ptr_metadata", "is_val_statically_known",
    "ub_checks", "size_of", "align_of", "min_align_of", "pref_align_of", "abort", "unreachable", "assume",
    "cold_path", "likely", "unlikely", "black_box", "transmute", "transmute_unchecked", "caller_location",
    "offset", "arith_offset", "ptr_offset_from", "ptr_offset_from_unsigned", "const_eval_select",
];

/// Cached per update: the same call targets appear in many bodies.
fn resolve<'db>(tcx: &TyCx<'db>, fid: FunctionId, args: GenericArgs<'db>) -> Result<Target<'db>> {
    if let Some(r) = tcx.resolved.borrow().get(&(fid, args)) {
        return r.clone().map_err(|e| anyhow!(e));
    }
    let r = resolve_uncached(tcx, fid, args);
    let stored = match &r {
        Ok(t) => Ok(t.clone()),
        Err(e) => Err(format!("{e:#}")),
    };
    tcx.resolved.borrow_mut().insert((fid, args), stored);
    r
}

fn resolve_uncached<'db>(tcx: &TyCx<'db>, fid: FunctionId, args: GenericArgs<'db>) -> Result<Target<'db>> {
    let db = tcx.db;
    let (fid, args) = if matches!(fid.lookup(db).container, ItemContainerId::TraitId(_)) {
        match db.lookup_impl_method(tcx.env, fid, args) {
            (Either::Left(f), a) => (f, a),
            (Either::Right(_), _) => bail!("call to a builtin-derive method (derive(Clone) etc.) unsupported"),
        }
    } else {
        (fid, args)
    };
    let f = hir::Function::from(fid);
    if ra_ap_hir_def::signatures::FunctionSignature::is_intrinsic(db, fid) {
        let name = f.name(db).as_str().to_owned();
        if INTRINSICS.contains(&name.as_str()) {
            return Ok(Target::Intrinsic(name, args));
        }
    }
    if !f.has_body(db) {
        let name = f.name(db).as_str().to_owned();
        return match HOST_FNS.iter().find(|(n, _)| *n == name) {
            Some((_, p)) => Ok(Target::Host(*p)),
            None => bail!("call to `{name}`: no body (trait method without impl, intrinsic or unknown extern fn)"),
        };
    }
    Ok(Target::Inst(fid, args))
}

fn inst_key<'db>(roots: &HashSet<FunctionId>, fid: FunctionId, args: GenericArgs<'db>) -> (FunctionId, String) {
    if roots.contains(&fid) || args.is_empty() {
        (fid, String::new())
    } else {
        (fid, format!("{args:?}"))
    }
}

/// Slot name for a non-root instance: module path, fn name, a disambiguator, generic args.
fn inst_name(db: &dyn HirDatabase, fid: FunctionId, args_key: &str) -> String {
    let f = hir::Function::from(fid);
    let mut s = String::new();
    for m in f.module(db).path_to_root(db).into_iter().rev() {
        if let Some(n) = m.name(db) {
            s.push_str(n.as_str());
            s.push_str("::");
        }
    }
    format!("{s}{}#{fid:?}{args_key}", f.name(db).as_str())
}

/// The guest file a fn lives in, None for sysroot fns (whose text never changes).
fn guest_file(db: &dyn HirDatabase, fid: FunctionId) -> Option<ra_ap_vfs::FileId> {
    let m = hir::Function::from(fid).module(db);
    if !m.krate(db).origin(db).is_local() {
        return None;
    }
    Some(m.definition_source_file_id(db).original_file(db).file_id(db))
}

/// Copy `size` bytes; small copies are inlined as loads/stores, larger go to memmove.
fn copy_mem(b: &mut FunctionBuilder, dst: Value, src: Value, size: u32, align: u32) {
    if size <= 128 {
        let chunk = align.clamp(1, 8);
        let t = match chunk {
            8 => types::I64,
            4 => types::I32,
            2 => types::I16,
            _ => types::I8,
        };
        let mut off = 0;
        while off + chunk <= size {
            let v = b.ins().load(t, flags(), src, off as i32);
            b.ins().store(flags(), v, dst, off as i32);
            off += chunk;
        }
        while off < size {
            let v = b.ins().load(types::I8, flags(), src, off as i32);
            b.ins().store(flags(), v, dst, off as i32);
            off += 1;
        }
        return;
    }
    let mut sig = Signature::new(b.func.signature.call_conv);
    for _ in 0..3 {
        sig.params.push(AbiParam::new(types::I64));
    }
    let sref = b.import_signature(sig);
    let f = b.ins().iconst(types::I64, host_memmove as *const u8 as i64);
    let n = b.ins().iconst(types::I64, size as i64);
    b.ins().call_indirect(sref, f, &[dst, src, n]);
}

/// MIR local 0 is the return place.
fn return_local(body: &MirBody<'_>) -> LocalId {
    body.locals.iter().next().expect("return local").0
}

/// Locals whose address is taken (`&local...` without a deref in the path) must live in memory.
fn address_taken(body: &MirBody<'_>) -> HashSet<LocalId> {
    let mut out = HashSet::new();
    for (_, bb) in body.basic_blocks.iter() {
        for st in &bb.statements {
            if let StatementKind::Assign(_, Rvalue::Ref(_, p)) = &st.kind {
                if !p.projection.as_slice().iter().any(|e| matches!(e, ProjectionElem::Deref)) {
                    out.insert(p.local);
                }
            }
        }
    }
    out
}

/// Guest ABI: [sret ptr if the return is an aggregate] ++ params (scalars by value, aggregates by ptr).
fn signature_of(tcx: &TyCx<'_>, body: &MirBody<'_>, module: &JITModule) -> Result<(Signature, bool)> {
    let mut sig = module.make_signature();
    let ret = tcx.repr(body.locals[return_local(body)].ty.as_ref())?;
    let sret = matches!(ret, Repr::Mem { .. });
    if sret {
        sig.params.push(AbiParam::new(types::I64));
    }
    for p in &body.param_locals {
        match tcx.repr(body.locals[*p].ty.as_ref())? {
            Repr::Zst => {}
            Repr::Scalar(ts) => sig.params.extend(ts.into_iter().map(AbiParam::new)),
            Repr::Mem { .. } => sig.params.push(AbiParam::new(types::I64)),
        }
    }
    if let Repr::Scalar(ts) = ret {
        sig.returns.extend(ts.into_iter().map(AbiParam::new));
    }
    Ok((sig, sret))
}

fn compile_fn(
    jit: &mut Jit,
    fctx: &mut FunctionBuilderContext,
    tcx: &TyCx<'_>,
    body: &MirBody<'_>,
    symbol: &str,
    cenv: &CompileEnv,
) -> Result<cranelift_module::FuncId> {
    let (sig, has_sret) = signature_of(tcx, body, &jit.module)?;
    let mut ctx = jit.module.make_context();
    let fcfg = jit.module.target_config();
    ctx.func.signature = sig.clone();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, fctx);
        // SAFETY: lifetime unification only; tcx and body both borrow the same db for this call.
        let body: &MirBody<'_> = unsafe { std::mem::transmute(body) };
        let mut lw = Lower { tcx, body, storage: HashMap::new(), blocks: HashMap::new(), cenv, sret: None };
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let taken = address_taken(body);
        for (l, local) in body.locals.iter() {
            let Ok(r) = tcx.repr(local.ty.as_ref()) else { continue };
            let st = match r {
                Repr::Zst => Storage::Vars(vec![]),
                Repr::Scalar(ts) if !taken.contains(&l) => {
                    let mut vs = vec![];
                    for t in ts {
                        let v = b.declare_var(t);
                        let z = if t == types::F64 {
                            b.ins().f64const(0.0)
                        } else if t == types::F32 {
                            b.ins().f32const(0.0)
                        } else {
                            b.ins().iconst(t, 0)
                        };
                        b.def_var(v, z);
                        vs.push(v);
                    }
                    Storage::Vars(vs)
                }
                Repr::Scalar(_) => {
                    Storage::Slot(b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3)))
                }
                Repr::Mem { size, align } => Storage::Slot(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    size,
                    align.max(1).trailing_zeros() as u8,
                ))),
            };
            lw.storage.insert(l, st);
        }
        let params: Vec<Value> = b.block_params(entry).to_vec();
        let mut pi = 0;
        if has_sret {
            lw.sret = Some(params[0]);
            pi = 1;
        }
        for p in &body.param_locals {
            let ty = lw.local_ty(*p);
            let v = match tcx.repr(ty)? {
                Repr::Zst => Val::S(vec![]),
                Repr::Scalar(ts) => {
                    let v = Val::S(params[pi..pi + ts.len()].to_vec());
                    pi += ts.len();
                    v
                }
                Repr::Mem { .. } => {
                    pi += 1;
                    Val::M(params[pi - 1])
                }
            };
            // Aggregates arrive by pointer and are copied into the callee's own slot.
            let loc = match lw.storage.get(p) {
                Some(Storage::Vars(_)) => Loc::Var(*p),
                Some(Storage::Slot(s)) => Loc::Addr(b.ins().stack_addr(types::I64, *s, 0), 0),
                None => bail!("param of unsupported type {ty:?}"),
            };
            lw.write(&mut b, loc, ty, v)?;
        }
        for (bb, _) in body.basic_blocks.iter() {
            lw.blocks.insert(bb, b.create_block());
        }
        b.ins().jump(lw.blocks[&body.start_block], &[]);

        for (bb, data) in body.basic_blocks.iter() {
            b.switch_to_block(lw.blocks[&bb]);
            let r: Result<()> = (|| {
                for st in &data.statements {
                    match &st.kind {
                        StatementKind::Assign(place, rv) => lw.assign(&mut b, place, rv)?,
                        StatementKind::FakeRead(_)
                        | StatementKind::Deinit(_)
                        | StatementKind::StorageLive(_)
                        | StatementKind::StorageDead(_)
                        | StatementKind::Nop => {}
                    }
                }
                let term = data.terminator.as_ref().ok_or_else(|| anyhow!("block without terminator"))?;
                match &term.kind {
                    TerminatorKind::Goto { target }
                    | TerminatorKind::FalseEdge { real_target: target, .. }
                    | TerminatorKind::FalseUnwind { real_target: target, .. }
                    | TerminatorKind::Drop { target, .. } => {
                        b.ins().jump(lw.blocks[target], &[]);
                    }
                    TerminatorKind::Return => {
                        let ret = return_local(body);
                        let ty = lw.local_ty(ret);
                        if let Some(sret) = lw.sret {
                            let v = lw.read_local(&mut b, ret, ty)?;
                            lw.write(&mut b, Loc::Addr(sret, 0), ty, v)?;
                            b.ins().return_(&[]);
                        } else {
                            let Val::S(vals) = lw.read_local(&mut b, ret, ty)? else { bail!("bad return") };
                            b.ins().return_(&vals);
                        }
                    }
                    TerminatorKind::SwitchInt { discr, targets } => {
                        let (v, _) = lw.scalar(&mut b, discr)?;
                        let mut sw = Switch::new();
                        for (val, t) in targets.iter() {
                            sw.set_entry(val, lw.blocks[&t]);
                        }
                        sw.emit(&mut b, v, lw.blocks[&targets.otherwise()]);
                    }
                    TerminatorKind::Assert { cond, expected, target, .. } => {
                        let (c, _) = lw.scalar(&mut b, cond)?;
                        let ok = lw.blocks[target];
                        let fail = b.create_block();
                        if *expected {
                            b.ins().brif(c, ok, &[], fail, &[]);
                        } else {
                            b.ins().brif(c, fail, &[], ok, &[]);
                        }
                        b.switch_to_block(fail);
                        b.ins().trap(TrapCode::unwrap_user(1));
                    }
                    TerminatorKind::Call { func, args, destination, target, .. } => {
                        let callee = lw.callee(func)?;
                        let dst = lw.place(&mut b, destination)?;
                        if let Callee::Ctor(v) = &callee {
                            let vals = args.iter().map(|a| lw.operand(&mut b, a)).collect::<Result<Vec<_>>>()?;
                            lw.write_fields(&mut b, &dst, *v, vals)?;
                            let t = target.ok_or_else(|| anyhow!("constructor call without target"))?;
                            b.ins().jump(lw.blocks[&t], &[]);
                            return Ok(());
                        }
                        if let Callee::Intrinsic(name, gargs) = &callee {
                            let diverges = lw.intrinsic(&mut b, name, *gargs, args, &dst)?;
                            match (diverges, target) {
                                (true, _) => {}
                                (false, Some(t)) => {
                                    b.ins().jump(lw.blocks[t], &[]);
                                }
                                (false, None) => {
                                    b.ins().trap(TrapCode::unwrap_user(2));
                                }
                            }
                            return Ok(());
                        }
                        let fptr = match callee {
                            Callee::Guest(slot) => lw.slot_ptr(&mut b, slot),
                            Callee::Host(p) => b.ins().iconst(types::I64, p as i64),
                            Callee::Intrinsic(..) | Callee::Ctor(_) => unreachable!(),
                        };
                        let vals = args.iter().map(|a| lw.operand(&mut b, a)).collect::<Result<Vec<_>>>()?;
                        lw.emit_call(&mut b, fptr, vals, &dst)?;
                        match target {
                            Some(t) => {
                                b.ins().jump(lw.blocks[t], &[]);
                            }
                            None => {
                                b.ins().trap(TrapCode::unwrap_user(2));
                            }
                        }
                    }
                    TerminatorKind::Unreachable | TerminatorKind::Abort | TerminatorKind::UnwindResume => {
                        b.ins().trap(TrapCode::unwrap_user(3));
                    }
                    other => bail!("terminator unsupported: {other:?}"),
                }
                Ok(())
            })();
            if let Err(e) = r {
                if data.is_cleanup {
                    // Cleanup (unwind) paths are never taken in this subset.
                    b.ins().trap(TrapCode::unwrap_user(4));
                } else {
                    return Err(e.context(format!("in block {bb:?}")));
                }
            }
        }
        b.seal_all_blocks();
        b.finalize(fcfg);
    }
    let id = jit.module.declare_function(symbol, Linkage::Local, &sig)?;
    jit.module
        .define_function(id, &mut ctx)
        .map_err(|e| anyhow!("cranelift define {symbol}: {e:?}"))?;
    jit.module.clear_context(&mut ctx);
    Ok(id)
}

// ---------------------------------------------------------------- dependency keys

/// One function instance to (maybe) compile in this update.
struct Unit<'db> {
    name: String,
    file: Option<ra_ap_vfs::FileId>,
    body: &'db MirBody<'db>,
    deps: Deps<'db>,
    key: (FunctionId, String),
}

/// What a body refers to outside itself; feeds the recompile key.
#[derive(Default)]
struct Deps<'db> {
    callees: Vec<(FunctionId, GenericArgs<'db>)>,
    consts: Vec<Const<'db>>,
    statics: Vec<StaticId>,
}

fn scan<'db>(body: &MirBody<'db>) -> Deps<'db> {
    let mut d = Deps::default();
    let op = |o: &Operand, d: &mut Deps<'db>| match &o.kind {
        OperandKind::Constant { konst, .. } => d.consts.push(konst.as_ref()),
        OperandKind::Static(s) => d.statics.push(*s),
        OperandKind::Allocation { allocation } => {
            if let TyKind::FnDef(def, args) = allocation.as_ref().ty.kind() {
                if let ra_ap_hir_def::CallableDefId::FunctionId(f) = def.0 {
                    d.callees.push((f, args));
                }
            }
        }
        _ => {}
    };
    // fn items passed as values (e.g. const_eval_select's runtime fn) are callees too
    for (_, l) in body.locals.iter() {
        if let TyKind::FnDef(def, args) = l.ty.as_ref().kind() {
            if let ra_ap_hir_def::CallableDefId::FunctionId(f) = def.0 {
                d.callees.push((f, args));
            }
        }
    }
    for (_, bb) in body.basic_blocks.iter() {
        for st in &bb.statements {
            if let StatementKind::Assign(_, rv) = &st.kind {
                match rv {
                    Rvalue::Use(o) | Rvalue::Repeat(o, _) | Rvalue::Cast(_, o, _) | Rvalue::UnaryOp(_, o) => op(o, &mut d),
                    Rvalue::CheckedBinaryOp(_, a, b) => {
                        op(a, &mut d);
                        op(b, &mut d);
                    }
                    Rvalue::Aggregate(_, os) => os.iter().for_each(|o| op(o, &mut d)),
                    _ => {}
                }
            }
        }
        if let Some(t) = &bb.terminator {
            match &t.kind {
                TerminatorKind::Call { func, args, .. } => {
                    op(func, &mut d);
                    args.iter().for_each(|o| op(o, &mut d));
                }
                TerminatorKind::SwitchInt { discr, .. } => op(discr, &mut d),
                TerminatorKind::Assert { cond, .. } => op(cond, &mut d),
                _ => {}
            }
        }
    }
    d
}

/// Signature key: param and return types plus their layouts. Callers hash their callees' keys,
/// so a signature change recompiles every caller even if the caller's text did not change.
fn sig_key(tcx: &TyCx<'_>, body: &MirBody<'_>) -> u64 {
    let mut s = String::new();
    for p in body.param_locals.iter().chain(std::iter::once(&return_local(body))) {
        tcx.layout_key(body.locals[*p].ty.as_ref(), &mut s);
    }
    hash_str(&s)
}

// ---------------------------------------------------------------- driver

struct EditStats {
    ra_ms: f64,
    cl_ms: f64,
    swap_ms: f64,
    recompiled: Vec<String>,
    total_fns: usize,
    errors: Vec<String>,
    notes: Vec<String>,
    prof: String,
}

/// MIR text for change detection. ra 0.0.358's pretty-printer panics on some f64 constants
/// (display.rs:863 `try_into().unwrap()`); fall back to the Debug form then.
fn body_text(db: &dyn HirDatabase, body: &MirBody<'_>, dt: ra_ap_hir_ty::display::DisplayTarget) -> String {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body.pretty_print(db, dt)));
    std::panic::set_hook(prev);
    r.unwrap_or_else(|_| format!("{body:?}"))
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn struct_fields<'db>(db: &'db dyn HirDatabase, tcx: &TyCx<'db>, ty: Ty<'db>) -> Vec<FieldInfo> {
    let TyKind::Adt(def, _) = ty.kind() else { return vec![] };
    let ra_ap_hir_def::AdtId::StructId(sid) = def.def_id() else { return vec![] };
    let Ok(l) = tcx.layout(ty) else { return vec![] };
    let mut out = vec![];
    for (i, f) in hir::Struct::from(sid).fields(db).into_iter().enumerate() {
        let fe = ProjectionElem::<LocalId>::Field(FieldIndex(i as u32));
        let fty = tcx.proj(PlaceTy::from_ty(ty), &fe).ty;
        let mut key = String::new();
        tcx.layout_key(fty, &mut key);
        let Ok(size) = tcx.size_of(fty) else { continue };
        out.push(FieldInfo { name: f.name(db).as_str().to_owned(), offset: l.fields.offset(i).bytes(), size, key });
    }
    out
}

/// Allocate (once) the host memory for a static and return the address of its cell. If its
/// layout changed, allocate new memory from the initializer and schedule a swap that copies
/// over every top-level field whose name and layout are unchanged.
fn ensure_static<'db>(
    jit: &mut Jit,
    db: &'db dyn HirDatabase,
    tcx: &TyCx<'db>,
    sid: StaticId,
    notes: &mut Vec<String>,
) -> Result<usize> {
    let name = hir::Static::from(sid).name(db).as_str().to_owned();
    let init = db.const_eval_static(sid).map_err(|e| anyhow!("static {name}: initializer: {e:?}"))?;
    let mut key = String::new();
    tcx.layout_key(init.ty, &mut key);
    if let Some(c) = jit.statics.get(&name) {
        if c.key == key {
            return Ok(c.cell as *const AtomicUsize as usize);
        }
    }
    if !matches!(init.memory_map, MemoryMap::Empty) {
        bail!("static {name}: initializer with pointers unsupported");
    }
    let size = init.memory.len();
    let addr = leak_aligned(&init.memory, size);
    let fields = struct_fields(db, tcx, init.ty);
    if let Some(c) = jit.statics.get_mut(&name) {
        let mut copies = vec![];
        let (mut kept, mut reset) = (vec![], vec![]);
        for nf in &fields {
            match c.fields.iter().find(|of| of.name == nf.name && of.key == nf.key && of.size == nf.size) {
                Some(of) => {
                    copies.push((of.offset, nf.offset, nf.size));
                    kept.push(nf.name.clone());
                }
                None => reset.push(nf.name.clone()),
            }
        }
        notes.push(format!("static {name}: layout changed, re-allocated; state kept for {kept:?}, initialised {reset:?}"));
        jit.pending_statics.push(PendingStatic { cell: c.cell, new_addr: addr, copies });
        c.key = key;
        c.fields = fields;
        return Ok(c.cell as *const AtomicUsize as usize);
    }
    let cell: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(addr)));
    jit.statics.insert(name, StaticCell { cell, key, fields });
    Ok(cell as *const AtomicUsize as usize)
}

/// Layout key, size and alignment of the state type `S` of `fn init() -> S` / `fn frame(&mut S)`.
fn state_plan<'db>(tcx: &TyCx<'db>, units: &[Unit<'db>], init: &str, frame: &str) -> Result<(String, usize, usize)> {
    let find = |n: &str| units.iter().find(|u| u.name == n).ok_or_else(|| anyhow!("guest has no fn `{n}`"));
    let (ib, fb) = (find(init)?.body, find(frame)?.body);
    let ty = ib.locals[return_local(ib)].ty.as_ref();
    if !matches!(tcx.repr(ty)?, Repr::Mem { .. }) {
        bail!("`{init}` must return a struct (the state)");
    }
    let ok = fb.param_locals.len() == 1
        && matches!(fb.locals[fb.param_locals[0]].ty.as_ref().kind(), TyKind::Ref(_, t, _) if t == ty);
    if !ok {
        bail!("`{frame}` must take exactly `&mut S`, where `S` is what `{init}` returns");
    }
    let mut key = String::new();
    tcx.layout_key(ty, &mut key);
    let l = tcx.layout(ty)?;
    Ok((key, l.size.bytes() as usize, l.align.abi.bytes() as usize))
}

/// Re-query MIR for every guest fn, recompile the ones whose key changed, swap slots.
/// `changed`: files edited since the last update (None = all). The MIR text of a fn in an
/// unchanged file is not re-printed; its key still picks up changed consts, callee signatures,
/// callee identities, layouts and statics, which is how edits elsewhere reach it.
fn update(front: &Front, jit: &mut Jit, changed: Option<&HashSet<ra_ap_vfs::FileId>>) -> Result<EditStats> {
    let db: &dyn HirDatabase = &front.db;
    ra_ap_hir_ty::attach_db(db, || {
        let t0 = Instant::now();
        let (fns, krate) = front.guest_fns()?;
        let p_fns = t0.elapsed().as_secs_f64() * 1e3;
        let tcx = TyCx::new(db, krate);
        let dt = krate.to_display_target(db);
        let mut errors = vec![];
        let mut notes = vec![];
        let mut cenv = CompileEnv {
            roots: fns.iter().map(|f| f.fid).collect(),
            slots: HashMap::new(),
            statics: HashMap::new(),
            table_addr: jit.table.as_ptr() as i64,
        };
        // Worklist: the guest's non-generic fns, then every instance they reach (generic guest
        // fns at concrete types, trait methods resolved to impls, sysroot fns), monomorphized.
        let mut units: Vec<Unit> = vec![];
        let mut seen = HashSet::new();
        for f in &fns {
            match db.mir_body(f.def) {
                Ok(body) => {
                    let key = (f.fid, String::new());
                    seen.insert(key.clone());
                    units.push(Unit { name: f.name.clone(), file: Some(f.file), body, deps: scan(body), key });
                }
                Err(e) => errors.push(format!("fn {}: MIR lowering failed: {e:?}", f.name)),
            }
        }
        let mut i = 0;
        while i < units.len() {
            let callees = units[i].deps.callees.clone();
            for (fid, args) in callees {
                let Ok(Target::Inst(f2, a2)) = resolve(&tcx, fid, args) else { continue };
                let key = inst_key(&cenv.roots, f2, a2);
                if !seen.insert(key.clone()) {
                    continue;
                }
                let name = inst_name(db, f2, &key.1);
                let def = InferBodyId::DefWithBodyId(ra_ap_hir_def::DefWithBodyId::FunctionId(f2));
                match db.monomorphized_mir_body(def, a2.store(), tcx.env.store()) {
                    Ok(body) => units.push(Unit { name, file: guest_file(db, f2), body, deps: scan(body), key }),
                    Err(e) => errors.push(format!("instance {name}: MIR lowering failed: {e:?}")),
                }
            }
            i += 1;
        }
        let mut sigs = HashMap::new();
        for u in &units {
            let s = jit.slot(&u.name);
            cenv.slots.insert(u.key.clone(), s);
            sigs.insert(u.key.clone(), sig_key(&tcx, u.body));
        }
        let p_mir = t0.elapsed().as_secs_f64() * 1e3;
        let mut p_pp = 0.0;
        let mut keyed = vec![];
        for u in &units {
            let (body, deps) = (u.body, &u.deps);
            let tp = Instant::now();
            let unchanged_file = changed.is_some_and(|c| u.file.is_none_or(|f| !c.contains(&f)));
            let th = match jit.text_hashes.get(&u.name) {
                Some(h) if unchanged_file => *h,
                _ => {
                    let h = hash_str(&body_text(db, body, dt));
                    jit.text_hashes.insert(u.name.clone(), h);
                    h
                }
            };
            let mut key = format!("{th:x};");
            p_pp += tp.elapsed().as_secs_f64() * 1e3;
            for (_, l) in body.locals.iter() {
                tcx.layout_key(l.ty.as_ref(), &mut key);
            }
            for c in &deps.consts {
                match tcx.eval_const(*c) {
                    Ok(a) => {
                        let _ = write!(key, "const {:?};", &a.memory[..]);
                    }
                    Err(e) => {
                        let _ = write!(key, "const err {e};");
                    }
                }
            }
            for s in &deps.statics {
                match ensure_static(jit, db, &tcx, *s, &mut notes) {
                    Ok(cell) => {
                        cenv.statics.insert(*s, cell);
                        let _ = write!(key, "static {cell:x};");
                    }
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            for (fid, args) in &deps.callees {
                match resolve(&tcx, *fid, *args) {
                    Ok(Target::Inst(f2, a2)) => {
                        let k = inst_key(&cenv.roots, f2, a2);
                        let _ = write!(key, "callee {:?} {:x?};", cenv.slots.get(&k), sigs.get(&k));
                    }
                    Ok(Target::Host(p)) => {
                        let _ = write!(key, "host {p:?};");
                    }
                    Ok(Target::Intrinsic(n, a)) => {
                        let _ = write!(key, "intrinsic {n} {a:?};");
                    }
                    Err(e) => {
                        let _ = write!(key, "callee err {e};");
                    }
                }
            }
            keyed.push((u, hash_str(&key)));
        }
        let ra_ms = t0.elapsed().as_secs_f64() * 1e3;
        let prof = format!("fns={p_fns:.2} mir+sig={:.2} pretty={p_pp:.2} keys={:.2}", p_mir - p_fns, ra_ms - p_mir - p_pp);

        let t1 = Instant::now();
        jit.generation += 1;
        let mut new = vec![];
        for (u, h) in &keyed {
            if jit.hashes.get(&u.name) == Some(h) {
                continue;
            }
            let sym = format!("{}$g{}", u.name, jit.generation);
            let mut fctx = FunctionBuilderContext::new();
            match compile_fn(jit, &mut fctx, &tcx, u.body, &sym, &cenv) {
                Ok(id) => new.push((u.name.clone(), id, *h)),
                Err(e) => errors.push(format!("compiling fn `{}`: {e:#}", u.name)),
            }
        }
        let cl_ms = t1.elapsed().as_secs_f64() * 1e3;

        let t2 = Instant::now();
        jit.module.finalize_definitions()?;
        // Host-owned state: allocate on first sight; if `S`'s layout changed, allocate fresh
        // zeroed memory and run the new `init` into it, published together with the new code.
        let mut plans: Vec<(usize, String, usize, usize)> = vec![];
        for (si, (init, frame)) in SYSTEMS.iter().enumerate() {
            let (key, size, align) = match state_plan(&tcx, &units, init, frame) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(format!("state `{init}`/`{frame}`: {e:#}"));
                    continue;
                }
            };
            if jit.states.get(si).is_some_and(|s| s.key == key) {
                continue;
            }
            if errors.iter().any(|e| e.starts_with(&format!("compiling fn `{init}`")) || e.starts_with(&format!("compiling fn `{frame}`"))) {
                continue; // retried on the next edit; the old state keeps running with the old code
            }
            plans.push((si, key, size, align));
        }
        let _pair = STATE_PAIR.lock().unwrap();
        // Statics first (old frames still running will pick up the new memory through the
        // cell), then code. Fields are copied at the last moment to lose as few writes as possible.
        for p in jit.pending_statics.drain(..) {
            let old = p.cell.load(Ordering::Acquire);
            for (from, to, n) in p.copies {
                unsafe { std::ptr::copy_nonoverlapping((old + from as usize) as *const u8, (p.new_addr + to as usize) as *mut u8, n as usize) };
            }
            p.cell.store(p.new_addr, Ordering::Release);
        }
        for (name, id, h) in &new {
            let p = jit.module.get_finalized_function(*id) as usize;
            jit.table[jit.slots[name]].store(p, Ordering::Release);
            jit.hashes.insert(name.clone(), *h);
        }
        // `init` needs its callees' new code live, so it runs after the table swap, still under
        // STATE_PAIR: the frame loop cannot pair the new code with the old memory.
        for (si, key, size, align) in plans {
            let (init, _) = SYSTEMS[si];
            let mem = unsafe { std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align(size.max(1), align.max(8))?) };
            let f: extern "C" fn(*mut u8) = unsafe { std::mem::transmute(jit.table[jit.slots[init]].load(Ordering::Acquire)) };
            f(mem);
            if si < jit.states.len() {
                notes.push(format!("state reset: layout of the `{init}` state changed, re-ran `{init}`"));
                jit.states[si].key = key;
                jit.states[si].cell.store(mem as usize, Ordering::Release);
            } else {
                let cell: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(mem as usize)));
                jit.states.push(StateSlot { key, cell });
            }
        }
        let swap_ms = t2.elapsed().as_secs_f64() * 1e3;
        Ok(EditStats {
            ra_ms,
            cl_ms,
            swap_ms,
            recompiled: new.into_iter().map(|(n, _, _)| n).collect(),
            total_fns: keyed.len(),
            errors,
            notes,
            prof,
        })
    })
}

fn dump_mir(front: &Front) -> Result<()> {
    let db: &dyn HirDatabase = &front.db;
    ra_ap_hir_ty::attach_db(db, || {
        let (fns, krate) = front.guest_fns()?;
        for f in fns {
            match db.mir_body(f.def) {
                Ok(b) => eprintln!("== {}\n{}", f.name, body_text(db, b, krate.to_display_target(db))),
                Err(e) => eprintln!("== {}: {e:?}", f.name),
            }
        }
        Ok(())
    })
}

fn report(st: &EditStats) {
    // Errors in sysroot/generic instances (names with '#') only matter if that code runs: most
    // are cold panic paths. A call into one traps via host_missing with a message.
    let verbose = std::env::var_os("RAJIT_VERBOSE").is_some();
    let (inst, guest): (Vec<&String>, Vec<&String>) = st.errors.iter().partition(|e| e.contains('#'));
    for e in guest {
        eprintln!("  error: {e}");
    }
    if !inst.is_empty() {
        let shown = if verbose { inst.len() } else { 2.min(inst.len()) };
        for e in &inst[..shown] {
            eprintln!("  error (instance, fatal only if called): {e}");
        }
        if shown < inst.len() {
            eprintln!("  ... {} more instance errors (RAJIT_VERBOSE=1 to list)", inst.len() - shown);
        }
    }
    for n in &st.notes {
        eprintln!("  note: {n}");
    }
}

const LIFE_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("../life/Cargo.toml")),
    ("src/main.rs", include_str!("../life/src/main.rs")),
    ("src/host.rs", include_str!("../life/src/host.rs")),
];

/// `ra-jit life [dir]`: unpack the Game of Life guest (never overwriting edits), run the
/// compile/watch loop on a thread and the window on the main thread.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("life") {
        return run(args);
    }
    let dir = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("ra-jit-life"));
    for (name, text) in LIFE_FILES {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, text)?;
        }
    }
    let record = args.iter().position(|a| a == "--record").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let timeline = args.iter().position(|a| a == "--timeline").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    if record.is_some() {
        demo::VIRTUAL.store(true, Ordering::Relaxed);
    }
    eprintln!("editing {}/src/main.rs (also editable from any other editor)", dir.display());
    let d = dir.to_string_lossy().into_owned();
    std::thread::spawn(move || {
        if let Err(e) = run(vec!["ra-jit".into(), d, "--gui".into()]) {
            eprintln!("ra-jit: {e:#}");
            demo::with_status(|s| s.error = format!("{e:#}"));
        }
    });
    if let Some(out) = record {
        let timeline = timeline.ok_or_else(|| anyhow!("--record needs --timeline <file>"))?;
        return record::run(dir.join("src/main.rs"), &out, &timeline);
    }
    demo::run(dir.join("src/main.rs")).map_err(|e| anyhow!("window: {e}"))
}

fn run(args: Vec<String>) -> Result<()> {
    let gui = args.iter().any(|a| a == "--gui");
    let guest = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("guest"));
    let once = args.iter().any(|a| a == "--once");
    let dump = args.iter().any(|a| a == "--dump-mir");

    let t_start = Instant::now();
    let mut front = Front::load(&guest)?;
    let load_ms = t_start.elapsed().as_secs_f64() * 1e3;
    if dump {
        return dump_mir(&front);
    }
    let mut jit = Jit::new()?;
    let st = update(&front, &mut jit, None)?;
    eprintln!(
        "[cold] ra workspace+sysroot load {load_ms:.1} ms | first MIR+keys {:.1} ms | cranelift {:.1} ms ({} of {} fns) | finalize {:.2} ms | total {:.1} ms",
        st.ra_ms,
        st.cl_ms,
        st.recompiled.len(),
        st.total_fns,
        st.swap_ms,
        t_start.elapsed().as_secs_f64() * 1e3
    );
    let inst: Vec<&String> = st.recompiled.iter().filter(|n| n.contains('#')).collect();
    if !inst.is_empty() {
        let short: Vec<String> = inst.iter().take(4).map(|n| n.chars().take(60).collect()).collect();
        eprintln!("  {} instances (generic / trait-resolved / sysroot), e.g. {short:?}", inst.len());
    }
    report(&st);
    if gui {
        demo::with_status(|d| {
            d.cold_ms = t_start.elapsed().as_secs_f64() * 1e3;
            d.total_fns = st.total_fns;
            d.error = st.errors.join("\n");
        });
    }
    if jit.states.len() != SYSTEMS.len() {
        bail!("guest needs `fn init() -> S` and `fn frame(s: &mut S)`: {}", st.errors.join("; "));
    }
    let table = jit.table;
    let frames: Vec<(usize, &'static AtomicUsize)> =
        SYSTEMS.iter().zip(&jit.states).map(|((_, f), s)| (jit.slots[*f], s.cell)).collect();
    let run_frames = move |n: Option<u32>| {
        let mut i = 0;
        while n.is_none_or(|n| i < n) {
            for (slot, cell) in &frames {
                let (code, state) = {
                    let _g = STATE_PAIR.lock().unwrap();
                    (table[*slot].load(Ordering::Acquire), cell.load(Ordering::Acquire))
                };
                let f: extern "C" fn(usize) = unsafe { std::mem::transmute(code) };
                f(state);
            }
            i += 1;
        }
    };
    if once {
        let t = Instant::now();
        run_frames(Some(3));
        eprintln!(
            "[once] 3 frames ran in {:.1} ms; process total {:.1} ms",
            t.elapsed().as_secs_f64() * 1e3,
            t_start.elapsed().as_secs_f64() * 1e3
        );
        return Ok(());
    }

    std::thread::spawn(move || run_frames(None));

    let mut edits = 0u64;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(2));
        if gui && demo::WARM.swap(false, Ordering::Relaxed) {
            // Entering rustc mode: warm cargo's incremental cache so the first edit is not a cold build.
            let _ = std::process::Command::new("cargo").args(["build", "--quiet"]).current_dir(&guest).output();
        }
        let t_detect = Instant::now();
        let (changed, mtime) = front.poll();
        if changed.is_empty() {
            continue;
        }
        if gui && demo::RUSTC_MODE.load(Ordering::Relaxed) {
            // Behave like a rustc dev loop: the new code only goes live after a real build succeeds.
            let t = Instant::now();
            let out = std::process::Command::new("cargo").args(["build", "--quiet"]).current_dir(&guest).output();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            match out {
                Ok(o) if o.status.success() => demo::with_status(|d| d.cargo_ms = ms),
                Ok(o) => {
                    demo::take_edit_start();
                    let err = String::from_utf8_lossy(&o.stderr).into_owned();
                    demo::with_status(|d| {
                        d.cargo_ms = ms;
                        d.error = err;
                    });
                    continue;
                }
                Err(e) => {
                    demo::with_status(|d| d.error = format!("cargo: {e}"));
                    continue;
                }
            }
        }
        let set_ms = t_detect.elapsed().as_secs_f64() * 1e3;
        edits += 1;
        match update(&front, &mut jit, Some(&changed)) {
            Ok(st) => {
                let total = t_detect.elapsed().as_secs_f64() * 1e3;
                let from_mtime = mtime
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .map_or(total, |d| d.as_secs_f64() * 1e3);
                eprintln!(
                    "[edit {edits}] RAJIT ra_set={set_ms:.2}ms ra_mir={:.2}ms cranelift={:.2}ms swap={:.3}ms total={total:.2}ms from_mtime={:.2}ms recompiled={}/{} {:?}",
                    st.ra_ms,
                    st.cl_ms,
                    st.swap_ms,
                    from_mtime,
                    st.recompiled.len(),
                    st.total_fns,
                    st.recompiled
                );
                eprintln!("  prof: {}", st.prof);
                report(&st);
                if gui {
                    let ms = demo::take_edit_start().map_or(from_mtime, |t| t.elapsed().as_secs_f64() * 1e3);
                    demo::with_status(|d| {
                        d.edits = edits;
                        d.last_ms = ms;
                        d.total_fns = st.total_fns;
                        if !st.notes.is_empty() {
                            d.note = st.notes.join("; ");
                        }
                        d.recompiled = st.recompiled.iter().filter(|n| !n.contains('#')).cloned().collect();
                        d.error = st.errors.iter().filter(|e| !e.contains('#')).cloned().collect::<Vec<_>>().join("\n");
                    });
                }
            }
            Err(e) => {
                eprintln!("[edit {edits}] error, keeping old code: {e:#}");
                if gui {
                    demo::take_edit_start();
                    demo::with_status(|d| d.error = format!("{e:#}"));
                }
            }
        }
    }
}
