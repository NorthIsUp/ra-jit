//! Functions compiled again while the program runs, at the level the time spent in them
//! earns them (issue #6). Every function starts at level 1, Cranelift's code, which is what
//! a build and an edit make. The daemon samples the running program and asks for the hot
//! ones at a higher level:
//!
//! 1. **Cranelift**: the code of every build.
//! 2. **LLVM -O3**: the same IR with the hot functions it calls built into it, across crates
//!    and the standard library alike, optimized by LLVM. It counts how often it is entered
//!    and which way each two-way branch goes, in [`ra_jit_live_counters`].
//! 3. **LLVM -O3 + PGO**: the same IR again, without the counting, with what level 2 counted
//!    given to LLVM as branch weights and an entry count (`!prof`).
//!
//! The code of a level above 1 goes into a patch like any edit's: the patch defines what
//! differs from the last full build, and a tiered function differs. It is not kept in the
//! store, as a profile is not something an entry can be keyed by.
//!
//! An edit wins. Code of a level above 1 is kept with the print of the Cranelift code of the
//! function and of every function built into it; when one of those is another, the function
//! is at level 1 again in that same build, and [`Tiers::take_demoted`] says so.
//!
//! [`ra_jit_live_counters`]: https://docs.rs/ra-jit-live

use std::{
    cell::RefCell,
    sync::{Arc, Mutex},
    time::Duration,
};

use cranelift_codegen::ir::{self, InstBuilder, Opcode, types};
use hir_def::{FunctionId, attrs::AttrFlags};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{db::HirDatabase, next_solver::StoredGenericArgs, traits::StoredParamEnvAndCrate};

use super::{
    CodegenError, Instance, InstanceDef, Result,
    fragment::{CompiledFunction, Recorder},
    instance_serial, root_serial,
};

/// The symbol of the function that hands out the counters, which `ra-jit-live` defines in
/// every program that takes edits.
pub const COUNTERS: &str = "ra_jit_live_counters";

/// How deep functions are built into each other; how many instructions a hot body built in
/// may have, and any other; and how many all that is built into one function may add up to.
const DEPTH: u32 = 10;
const HOT_INSTRUCTIONS: usize = 4000;
const SMALL_INSTRUCTIONS: usize = 600;
const BUDGET: usize = 40_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Cranelift = 1,
    Llvm = 2,
    Pgo = 3,
}

impl Level {
    pub fn number(self) -> u8 {
        self as u8
    }
}

/// A function of the last full build, as the daemon asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Node(pub(super) Kind);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Root(FunctionId),
    Instance(Instance, StoredParamEnvAndCrate),
}

impl Node {
    fn serial(&self, db: &dyn HirDatabase) -> u128 {
        match &self.0 {
            Kind::Root(func) => root_serial(db, *func),
            Kind::Instance(instance, env) => instance_serial(db, instance.clone(), env.clone()),
        }
    }
}

/// What level 2 counted of a function: how often it was entered, and for each two-way branch
/// of its IR in order, how often it went each way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branches {
    pub entered: u64,
    pub taken: Vec<[u64; 2]>,
}

/// What the daemon asks of one function.
#[derive(Debug, Clone)]
pub struct Asked {
    pub node: Node,
    pub level: Level,
    /// The hot functions that may be built into it where it calls them.
    pub hot: Vec<Node>,
    /// At level 2, the first of the counters it counts in, when there are any to spare.
    pub counters: Option<u32>,
    /// At level 3, what level 2 counted.
    pub profile: Option<Branches>,
}

/// The code of a function at a level above 1.
pub struct Tiered {
    pub level: Level,
    code: CompiledFunction,
    /// The print of its Cranelift code, and of the code of each function built into it, when
    /// it was compiled: if one is another, an edit changed it.
    serial: u128,
    inlined: Vec<(Node, u128)>,
    /// How many two-way branches its IR has, and the first counter it counts in.
    pub branches: u32,
    pub counters: Option<u32>,
    /// How many functions were built into it, and how long it took.
    pub inlined_count: usize,
    pub took: Duration,
}

impl Tiered {
    /// How many counters from [`Tiered::counters`] on it uses: the entry, then two for each
    /// branch.
    pub fn counters_used(&self) -> u32 {
        self.counters.map_or(0, |_| 1 + 2 * self.branches)
    }
}

/// The functions of a program at a level above 1.
#[derive(Default, Clone)]
pub struct Tiers {
    nodes: FxHashMap<Node, Arc<Tiered>>,
    /// The functions an edit sent back to level 1, and why, since this was last asked.
    demoted: Arc<Mutex<Vec<(Node, String)>>>,
}

impl Tiers {
    pub fn install(&mut self, node: Node, tiered: Tiered) {
        self.nodes.insert(node, Arc::new(tiered));
    }

    pub fn level(&self, node: &Node) -> Level {
        self.nodes.get(node).map_or(Level::Cranelift, |it| it.level)
    }

    pub fn get(&self, node: &Node) -> Option<&Tiered> {
        self.nodes.get(node).map(|it| &**it)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Sends every function back to level 1, as a full build does.
    pub fn clear(&mut self) {
        let mut demoted = self.demoted.lock().unwrap_or_else(|e| e.into_inner());
        for (node, _) in self.nodes.drain() {
            demoted.push((node, "the program was linked again".to_owned()));
        }
    }

    /// The functions a build since the last call found edited, which are at level 1 again.
    pub fn take_demoted(&mut self) -> Vec<(Node, String)> {
        let demoted = std::mem::take(&mut *self.demoted.lock().unwrap_or_else(|e| e.into_inner()));
        for (node, _) in &demoted {
            self.nodes.remove(node);
        }
        demoted
    }

    pub(super) fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.keys()
    }

    /// The code of `node` at its level, unless it has none above 1 or an edit changed what
    /// that code was compiled from.
    pub(super) fn code(&self, db: &dyn HirDatabase, node: &Node) -> Option<&CompiledFunction> {
        let tiered = self.nodes.get(node)?;
        let edited = if node.serial(db) != tiered.serial {
            Some("edited")
        } else if tiered.inlined.iter().any(|(inlined, serial)| inlined.serial(db) != *serial) {
            Some("a function built into it was edited")
        } else {
            None
        };
        if let Some(why) = edited {
            let mut demoted = self.demoted.lock().unwrap_or_else(|e| e.into_inner());
            if !demoted.iter().any(|(it, _)| it == node) {
                demoted.push((node.clone(), why.to_owned()));
            }
            return None;
        }
        Some(&tiered.code)
    }
}

/// What a compile at a level above 1 builds in and counts.
pub(super) struct Request {
    level: Level,
    counters: Option<u32>,
    profile: Option<Branches>,
    branches: std::cell::Cell<u32>,
}

/// The hot functions the compile under way on this thread may build into what it compiles.
struct Inlining {
    roots: FxHashSet<FunctionId>,
    /// By what they are and their generic arguments: whether a call checks overflow is the
    /// caller's to say, which the last build may have settled otherwise.
    instances: FxHashSet<(InstanceDef, StoredGenericArgs)>,
    inlined: Vec<(Instance, StoredParamEnvAndCrate)>,
    depth: u32,
    /// How many instructions were built in so far.
    spent: usize,
    /// The bodies built so far, by what they are of: `None` for one that is not built in.
    built: FxHashMap<
        (InstanceDef, StoredGenericArgs, bool),
        Option<std::rc::Rc<super::fragment::Inlinable>>,
    >,
}

thread_local! {
    static INLINING: RefCell<Option<Inlining>> = const { RefCell::new(None) };
}

/// Runs `work` with no compile at a level above 1 under way on the thread, for what the
/// database keeps: its result must not depend on what that compile builds in.
pub(super) fn plain<T>(work: impl FnOnce() -> T) -> T {
    let held = INLINING.with(|it| it.borrow_mut().take());
    let out = work();
    INLINING.with(|it| *it.borrow_mut() = held);
    out
}

/// How large a body of `instance` the compile under way may build into its caller, if it may
/// build it in at all: a hot one up to [`HOT_INSTRUCTIONS`], any other up to
/// [`SMALL_INSTRUCTIONS`], as rustc builds in what is small. The body is then built with
/// [`hot_body`].
pub(super) fn inline_limit(db: &dyn HirDatabase, instance: &Instance) -> Option<usize> {
    let func = match instance.def {
        InstanceDef::Fn(func) => Some(func),
        InstanceDef::Closure(_) => None,
        _ => return None,
    };
    INLINING.with(|it| {
        let it = it.borrow();
        let inlining = it.as_ref()?;
        if inlining.depth >= DEPTH || inlining.spent >= BUDGET {
            return None;
        }
        if func
            .is_some_and(|func| AttrFlags::query(db, func.into()).contains(AttrFlags::INLINE_NEVER))
        {
            return None;
        }
        let hot = inlining.instances.contains(&(instance.def, instance.args.clone()))
            || func.is_some_and(|func| {
                instance.args.as_ref().iter().all(|it| it.region().is_some())
                    && inlining.roots.contains(&func)
            });
        Some(if hot { HOT_INSTRUCTIONS } else { SMALL_INSTRUCTIONS })
    })
}

/// The body of a hot callee, built to be put where it is called, with the hot functions it
/// calls built into it in turn.
pub(super) fn hot_body(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    limit: usize,
    tracked: bool,
) -> Option<std::rc::Rc<super::fragment::Inlinable>> {
    let key = (instance.def, instance.args.clone(), tracked);
    let known = INLINING.with(|it| it.borrow().as_ref()?.built.get(&key).cloned());
    let body = match known {
        Some(body) => body,
        None => {
            let body = build_body(db, instance, env, limit, tracked).map(std::rc::Rc::new);
            INLINING.with(|it| {
                if let Some(it) = it.borrow_mut().as_mut() {
                    it.built.insert(key, body.clone());
                }
            });
            body
        }
    };
    let body = body?;
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            it.spent += body.func.dfg.num_insts();
            if !it.inlined.iter().any(|(known, _)| known == instance) {
                it.inlined.push((instance.clone(), env.clone()));
            }
        }
    });
    Some(body)
}

fn build_body(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    limit: usize,
    tracked: bool,
) -> Option<super::fragment::Inlinable> {
    let dump = std::env::var_os("RA_JIT_DUMP_TIER").is_some();
    let refused = |why: &str| {
        if dump {
            let name = match instance.def {
                InstanceDef::Fn(func) => {
                    hir_def::signatures::FunctionSignature::of(db, func).name.as_str().to_owned()
                }
                _ => "a closure".to_owned(),
            };
            eprintln!("tier: {name} is not built in: {why}");
        }
    };
    if let InstanceDef::Fn(func) = instance.def {
        let generic = db.mir_body(func.into()).ok()?;
        if generic.coroutine.is_some() {
            refused("it is a coroutine");
            return None;
        }
    }
    let Ok(body) = super::instance_body(db, instance, env.clone()) else {
        refused("no body");
        return None;
    };
    INLINING.with(|it| it.borrow_mut().as_mut().map(|it| it.depth += 1));
    let built = (|| {
        let mut cg = Recorder::new(
            db,
            &**super::target().ok()?,
            super::selected_target().ok()?,
            instance.checked,
        );
        cg.unlocated = true;
        let args = instance.args.clone();
        let mut ctx = super::build(&mut cg, env.as_ref(db), Some(&*body), tracked, |fx| {
            fx.args = Some(args);
            fx.codegen()
        })
        .map_err(|e| refused(&e.to_string()))
        .ok()?;
        if cg.naked.is_some() || cg.opaque {
            refused("naked or opaque");
            return None;
        }
        super::inline(&mut ctx, &cg.inlined).map_err(|e| refused(&e.to_string())).ok()?;
        if ctx.func.dfg.num_insts() > limit {
            refused("too large");
            return None;
        }
        let inlinable = super::fragment::Inlinable::of(ctx.func, cg.finish().0);
        if inlinable.is_none() {
            refused("its data is out of order");
        }
        inlinable
    })();
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            it.depth -= 1;
        }
    });
    built
}

/// Compiles the function `asked` names at the level it asks for.
pub fn compile(db: &dyn HirDatabase, asked: &Asked) -> std::result::Result<Tiered, String> {
    let start = std::time::Instant::now();
    if asked.level == Level::Cranelift {
        return Err("level 1 is what every build compiles".to_owned());
    }
    let serial = asked.node.serial(db);
    if serial == 0 {
        return Err("Cranelift does not compile it".to_owned());
    }
    let mut inlining = Inlining {
        roots: FxHashSet::default(),
        instances: FxHashSet::default(),
        inlined: Vec::new(),
        depth: 0,
        spent: 0,
        built: FxHashMap::default(),
    };
    for node in &asked.hot {
        match &node.0 {
            Kind::Root(func) => _ = inlining.roots.insert(*func),
            Kind::Instance(instance, _) => {
                _ = inlining.instances.insert((instance.def, instance.args.clone()))
            }
        }
    }
    let request = Request {
        level: asked.level,
        counters: asked.counters.filter(|_| asked.level == Level::Llvm),
        profile: asked.profile.clone().filter(|_| asked.level == Level::Pgo),
        branches: std::cell::Cell::new(0),
    };
    if std::env::var_os("RA_JIT_DUMP_TIER").is_some() {
        eprintln!(
            "tier: {:?} at {:?}: {} roots and {} instances may be built in",
            asked.node,
            asked.level,
            inlining.roots.len(),
            inlining.instances.len()
        );
        for (def, args) in &inlining.instances {
            if let InstanceDef::Fn(func) = def {
                let name =
                    hir_def::signatures::FunctionSignature::of(db, *func).name.as_str().to_owned();
                eprintln!("tier:   hot {name} {args:?}");
            }
        }
    }
    let held = INLINING.with(|it| it.borrow_mut().replace(inlining));
    let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &asked.node.0 {
        Kind::Root(func) => super::root_code(db, *func, Some(&request)),
        Kind::Instance(instance, env) => {
            super::instance_code(db, instance.clone(), env.clone(), Some(&request))
        }
    }));
    let inlining = INLINING.with(|it| std::mem::replace(&mut *it.borrow_mut(), held));
    let code = match compiled {
        Ok(code) => code.map_err(|e| e.to_string())?,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    let mut inlined = Vec::new();
    for (instance, env) in inlining.map(|it| it.inlined).unwrap_or_default() {
        let node = match instance.def {
            InstanceDef::Fn(func)
                if instance.args.as_ref().iter().all(|it| it.region().is_some()) =>
            {
                Node(Kind::Root(func))
            }
            _ => Node(Kind::Instance(instance, env)),
        };
        if !inlined.iter().any(|(it, _)| *it == node) {
            let serial = node.serial(db);
            inlined.push((node, serial));
        }
    }
    Ok(Tiered {
        level: asked.level,
        code,
        serial,
        inlined_count: inlined.len(),
        inlined,
        branches: request.branches.get(),
        counters: request.counters,
        took: start.elapsed(),
    })
}

/// The two-way branches of `func`, in the order of its layout.
fn branches(func: &ir::Function) -> Vec<ir::Inst> {
    func.layout
        .blocks()
        .filter_map(|block| func.layout.last_inst(block))
        .filter(|&inst| func.dfg.insts[inst].opcode() == Opcode::Brif)
        .collect()
}

/// Has `func` count, in the counters from `first` on, how often it is entered and which way
/// each of `branches` goes. The counters are found by a call at the entry.
fn count(
    recorder: &mut Recorder<'_>,
    func: &mut ir::Function,
    first: u32,
    branches: &[ir::Inst],
) -> Result<()> {
    use cranelift_codegen::cursor::{Cursor as _, FuncCursor};
    let pointer = super::ptr();
    let mut sig = recorder.make_signature();
    sig.returns.push(ir::AbiParam::new(pointer));
    let id = recorder.import(COUNTERS, &sig)?;
    let callee = recorder.declare_func_in_func(id, func);
    let Some(entry) = func.layout.entry_block() else {
        return Err(CodegenError::Unsupported("a function without blocks".to_owned()));
    };
    let flags = super::MEM;
    let mut cursor = FuncCursor::new(func).at_first_insertion_point(entry);
    let call = cursor.ins().call(callee, &[]);
    let base = cursor.func.dfg.first_result(call);
    let add = |cursor: &mut FuncCursor<'_>, offset: ir::Value| {
        let at = cursor.ins().iadd(base, offset);
        let count = cursor.ins().load(types::I64, flags, at, 0);
        let count = cursor.ins().iadd_imm_s(count, 1);
        cursor.ins().store(flags, count, at, 0);
    };
    let entered = cursor.ins().iconst(pointer, i64::from(first) * 8);
    add(&mut cursor, entered);
    for (nth, &inst) in branches.iter().enumerate() {
        let slot = i64::from(first) + 1 + 2 * nth as i64;
        cursor.goto_inst(inst);
        let condition = cursor.func.dfg.inst_args(inst)[0];
        let then = cursor.ins().iconst(pointer, slot * 8);
        let otherwise = cursor.ins().iconst(pointer, (slot + 1) * 8);
        let offset = cursor.ins().select(condition, then, otherwise);
        add(&mut cursor, offset);
    }
    Ok(())
}

/// The machine code of a function compiled at a level above 1: LLVM's, or an error where
/// LLVM has none, as the function then stays where it is.
pub(super) fn emit(
    recorder: &mut Recorder<'_>,
    ctx: &mut cranelift_codegen::Context,
    request: &Request,
) -> Result<super::fragment::Code> {
    let found = branches(&ctx.func);
    request.branches.set(found.len() as u32);
    let weights = match &request.profile {
        Some(profile) if profile.taken.len() == found.len() => Some(Weights {
            entered: profile.entered,
            taken: found.iter().copied().zip(profile.taken.iter().copied()).collect(),
        }),
        _ => None,
    };
    if let Some(first) = request.counters {
        count(recorder, &mut ctx.func, first, &found)?;
    }
    if std::env::var_os("RA_JIT_DUMP_TIER").is_some() {
        eprintln!("tier {:?}:\n{}", request.level, ctx.func.display());
    }
    produce(recorder, &ctx.func, weights.as_ref())
}

/// What level 3 tells LLVM: how often the function is entered, and how often each branch
/// goes each way.
#[cfg_attr(not(feature = "llvm"), allow(dead_code))]
pub(super) struct Weights {
    pub(super) entered: u64,
    pub(super) taken: FxHashMap<ir::Inst, [u64; 2]>,
}

#[cfg(feature = "llvm")]
fn produce(
    recorder: &mut Recorder<'_>,
    func: &ir::Function,
    weights: Option<&Weights>,
) -> Result<super::fragment::Code> {
    use super::backend::{Optimization, Passes, Selection};
    let optimization = Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
    let code = super::llvm::emit(recorder, func, optimization, weights)
        .map_err(|why| CodegenError::Unsupported(format!("LLVM has no code for it: {why}")))?;
    super::backend::pad(code)
}

#[cfg(not(feature = "llvm"))]
fn produce(
    _: &mut Recorder<'_>,
    _: &ir::Function,
    _: Option<&Weights>,
) -> Result<super::fragment::Code> {
    Err(CodegenError::Unsupported("ra-jit was built without `--features llvm`".to_owned()))
}
