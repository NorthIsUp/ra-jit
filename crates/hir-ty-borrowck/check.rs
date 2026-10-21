//! The walk over a body that reports errors, as `MirBorrowckCtxt` in rustc's
//! `rustc_borrowck/src/lib.rs`: every access to a place is checked for being initialized,
//! for being allowed to mutate what it mutates and, with the loans in scope, for conflicting
//! with a loan.

use hir_def::hir::{BindingId, ExprId, Pat, Statement};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::IntoKind;

use crate::{
    mir::{
        AsmOperand, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind, Operand, OperandKind,
        PlaceElem, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::TyKind,
};

use super::{
    BorrowckDiagnostic, Ctx,
    bitset::BitSet,
    cfg::{Cfg, Dominators, Location, index},
    moves::{InitFlow, InitKind, Lookup, MoveData, Mpi, apply},
};

pub(super) fn check<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    out: &mut Vec<BorrowckDiagnostic>,
) {
    let cfg = Cfg::new(body);
    let moves = MoveData::gather(ctx, body, &cfg);
    let flow = InitFlow::compute(body, &cfg, &moves);
    let dominators = Dominators::new(&cfg);
    let mut checker = Checker {
        ctx,
        body,
        cfg: &cfg,
        moves: &moves,
        dominators: &dominators,
        uninit: BitSet::default(),
        ever: BitSet::default(),
        errors: Vec::new(),
        access_reported: FxHashSet::default(),
        uninit_reported: FxHashSet::default(),
        move_reported: FxHashMap::default(),
        move_errors: Vec::new(),
        scrutinees: None,
        capture_temps: capture_temps(body),
        assigned_local: crate::mir::return_slot(),
        static_muts: static_muts(ctx, body),
        union_literals: union_literals(ctx),
    };
    for &block in &cfg.order {
        checker.uninit = flow.uninit_entry[index(block)].clone();
        checker.ever = flow.ever_entry[index(block)].clone();
        let data = &body.basic_blocks[block];
        for (statement, it) in data.statements.iter().enumerate() {
            let location = Location { block, statement };
            checker.statement(location, &it.kind, it.span);
            apply(body, &moves, location, &mut checker.uninit, &mut checker.ever);
        }
        if let Some(terminator) = &data.terminator {
            let location = Location { block, statement: data.statements.len() };
            checker.terminator(location, &terminator.kind, terminator.span);
        }
    }
    checker.report_move_errors();
    out.extend(checker.errors.into_iter().flatten());
    out.extend(checker.move_reported.into_values().map(|(_, _, error)| error));
}

fn union_literals(ctx: &Ctx<'_, '_>) -> FxHashSet<ExprId> {
    let mut found = FxHashSet::default();
    for (expr, it) in ctx.store.exprs() {
        if let hir_def::hir::Expr::RecordLit { fields, .. } = it {
            found.insert(expr);
            found.extend(fields.iter().map(|field| field.expr));
        }
    }
    found
}

/// A place as the checks pass it around.
#[derive(Clone, Copy)]
pub(super) struct PlaceRef<'p> {
    pub(super) local: LocalId,
    pub(super) projection: &'p [PlaceElem],
}

impl<'p> PlaceRef<'p> {
    fn of(place: &'p StoredPlace) -> Self {
        PlaceRef { local: place.local, projection: place.projection.as_slice() }
    }

    fn base(self, len: usize) -> Self {
        PlaceRef { local: self.local, projection: &self.projection[..len] }
    }

    fn key(self) -> (LocalId, Vec<PlaceElem>) {
        (self.local, self.projection.to_vec())
    }

    fn is_prefix_of(self, other: PlaceRef<'_>) -> bool {
        self.local == other.local && other.projection.starts_with(self.projection)
    }
}

/// What `is_mutable` says of a capture the closure may not mutate that rustc reports where the
/// closure is made.
const REPORTED_BY_PARENT: usize = usize::MAX - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Allowed {
    Yes,
    ExceptUpvars,
    No,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    Read,
    Mutate,
    MutableBorrow { closure_capture: bool },
    Move,
    StorageDeadOrDrop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InitAction {
    Use,
    Borrow,
    MatchOn,
    Assignment,
    PartialAssignment,
}

impl InitAction {
    fn noun(self) -> &'static str {
        match self {
            InitAction::Use => "use",
            InitAction::Borrow => "borrow",
            InitAction::MatchOn => "use",
            InitAction::Assignment => "assign",
            InitAction::PartialAssignment => "assign to part",
        }
    }

    fn as_general_verb(self) -> &'static str {
        match self {
            InitAction::Use | InitAction::MatchOn => "used",
            InitAction::Borrow => "borrowed",
            InitAction::Assignment => "assigned",
            InitAction::PartialAssignment => "partially assigned",
        }
    }
}

/// Why a move out of a place is not allowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MoveErrorKind {
    /// Out of what a reference or a pointer points to: the length of the place up to it.
    Borrowed(usize),
    /// Out of a field of a value whose type implements `Drop`: the length of the value.
    Destructor(usize),
    /// Out of an element of a slice or an array, at an index.
    SliceOrArray(usize),
}

struct MoveError {
    place: (LocalId, Vec<PlaceElem>),
    location: Location,
    span: MirSpan,
    kind: MoveErrorKind,
}

struct Checker<'a, 'b, 'db> {
    ctx: &'a Ctx<'b, 'db>,
    body: &'a MirBody<'db>,
    cfg: &'a Cfg,
    moves: &'a MoveData,
    dominators: &'a Dominators,
    uninit: BitSet,
    ever: BitSet,
    /// In the order reported; `None` for one a later error replaced.
    errors: Vec<Option<BorrowckDiagnostic>>,
    access_reported: FxHashSet<((LocalId, Vec<PlaceElem>), MirSpan)>,
    uninit_reported: FxHashSet<LocalId>,
    /// The `E0382` reported for a set of moves, with the place it is about.
    move_reported: FxHashMap<Vec<usize>, ((LocalId, Vec<PlaceElem>), Location, BorrowckDiagnostic)>,
    move_errors: Vec<MoveError>,
    scrutinees: Option<FxHashMap<BindingId, Option<ExprId>>>,
    /// The temporaries that hold a borrow a closure captures, with the span of the closure.
    capture_temps: FxHashMap<LocalId, MirSpan>,
    /// The local the statement being checked assigns, if it assigns one.
    assigned_local: LocalId,
    /// The temporaries that point to a `static mut`.
    static_muts: FxHashSet<LocalId>,
    /// Record literals and the values of their fields, whose lowering assigns a field of the
    /// literal where rustc assigns the whole of it (it matters for a union).
    union_literals: FxHashSet<ExprId>,
}

impl<'a, 'db> Checker<'a, '_, 'db> {
    fn report(&mut self, code: &'static str, span: MirSpan, message: String) {
        self.errors.push(Some(BorrowckDiagnostic { code, span, message }));
    }

    fn is_union_literal(&self, place: PlaceRef<'_>, span: MirSpan) -> bool {
        let MirSpan::ExprId(expr) = span else { return false };
        if !matches!(place.projection, [ProjectionElem::Field(_)])
            || !self.union_literals.contains(&expr)
        {
            return false;
        }
        let ty = self.body.locals[place.local].ty.as_ref();
        matches!(ty.kind(), TyKind::Adt(def, _) if def.is_union())
    }

    /// Whether the variable `place` is in, not behind a reference, is declared without `mut`.
    fn is_immutable_root(&self, place: PlaceRef<'_>) -> bool {
        if place.projection.contains(&ProjectionElem::Deref) {
            return false;
        }
        match self.ctx.upvar_of_local(place.local) {
            Some(upvar) => !self.ctx.upvars[upvar].mutable,
            None => self.ctx.is_immutable(place.local),
        }
    }

    fn describe(&self, place: PlaceRef<'_>) -> Option<String> {
        self.ctx.describe(self.body, place.local, place.projection)
    }

    fn quoted(&self, place: PlaceRef<'_>, otherwise: &str) -> String {
        match self.describe(place) {
            Some(name) => format!("`{name}`"),
            None => otherwise.to_owned(),
        }
    }

    fn statement(&mut self, location: Location, kind: &StatementKind, span: MirSpan) {
        match kind {
            StatementKind::Assign(place, rvalue) => {
                let place = PlaceRef::of(place);
                self.assigned_local = place.local;
                // Lowering copies each capture of a closure into a local of its own first,
                // which rustc uses in place.
                if span == MirSpan::Unknown
                    && place.projection.is_empty()
                    && self.ctx.upvar_of_local(place.local).is_some()
                {
                    return;
                }
                if let Rvalue::Ref(BorrowKind::Mut { kind: MutBorrowKind::Default }, borrowed) =
                    rvalue
                    && self.capture_temps.contains_key(&place.local)
                    && self.is_immutable_root(PlaceRef::of(borrowed))
                {
                    // rustc makes the mutable borrow of a variable without `mut` a closure
                    // captures a unique one, and reports the mutation in the closure.
                    let borrowed = PlaceRef::of(borrowed);
                    let access = Access::MutableBorrow { closure_capture: true };
                    self.access_place(location, borrowed, span, access, Allowed::No);
                    self.check_if_path_or_subpath_is_moved(
                        location,
                        InitAction::Borrow,
                        borrowed,
                        span,
                    );
                } else {
                    self.consume_rvalue(location, rvalue, span);
                }
                self.mutate_destination(location, place, span);
            }
            // rustc reads a matched place only where no arm matches, and the bindings of a
            // `let`; lowering reads the matched place itself and every initializer, so these
            // say nothing rustc checks.
            StatementKind::FakeRead(_) => (),
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { input, output, .. } => {
                            if let Some(input) = input {
                                self.consume_operand(location, input, span);
                            }
                            if let Some(output) = output {
                                self.mutate_place(location, PlaceRef::of(output), span);
                            }
                        }
                        AsmOperand::Const(value) | AsmOperand::Sym(value) => {
                            self.consume_operand(location, value, span)
                        }
                        AsmOperand::Label => (),
                    }
                }
            }
            StatementKind::StorageDead(local) => {
                let place = PlaceRef { local: *local, projection: &[] };
                self.access_place(location, place, span, Access::StorageDeadOrDrop, Allowed::Yes);
            }
            StatementKind::Deinit(_) | StatementKind::StorageLive(_) | StatementKind::Nop => (),
        }
    }

    fn terminator(&mut self, location: Location, kind: &TerminatorKind, span: MirSpan) {
        match kind {
            TerminatorKind::SwitchInt { discr, .. } => self.consume_operand(location, discr, span),
            TerminatorKind::Drop { place, .. } => {
                let place = PlaceRef::of(place);
                self.access_place(location, place, span, Access::StorageDeadOrDrop, Allowed::Yes);
            }
            TerminatorKind::DropAndReplace { place, value, .. } => {
                self.consume_operand(location, value, span);
                self.mutate_place(location, PlaceRef::of(place), span);
            }
            TerminatorKind::Call { func, args, destination, .. } => {
                self.consume_operand(location, func, span);
                for arg in args.iter() {
                    self.consume_operand(location, arg, span);
                }
                self.mutate_destination(location, PlaceRef::of(destination), span);
            }
            TerminatorKind::Assert { cond, msg, .. } => {
                self.consume_operand(location, cond, span);
                msg.for_each_operand(&mut |operand| {
                    if let Some(place) = operand_place(operand) {
                        self.check_if_path_or_subpath_is_moved(
                            location,
                            InitAction::Use,
                            place,
                            span,
                        );
                    }
                });
            }
            TerminatorKind::Yield { value, resume_arg, .. } => {
                self.consume_operand(location, value, span);
                self.mutate_place(location, PlaceRef::of(resume_arg), span);
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => (),
        }
    }

    fn mutate_destination(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        if self.is_union_literal(place, span) {
            // Lowering builds a union literal by assigning its field, where rustc assigns the
            // union.
            self.mutate_place(location, place.base(0), span);
        } else {
            self.mutate_place(location, place, span);
        }
    }

    fn consume_rvalue(&mut self, location: Location, rvalue: &Rvalue, span: MirSpan) {
        match rvalue {
            Rvalue::Ref(kind, place) => {
                let place = PlaceRef::of(place);
                let (access, allowed, action) = match kind {
                    BorrowKind::Shared => (Access::Read, Allowed::No, InitAction::Borrow),
                    BorrowKind::Shallow => (Access::Read, Allowed::No, InitAction::MatchOn),
                    BorrowKind::Mut { kind } => (
                        Access::MutableBorrow {
                            closure_capture: *kind == MutBorrowKind::ClosureCapture,
                        },
                        Allowed::No,
                        InitAction::Borrow,
                    ),
                };
                self.access_place(location, place, span, access, allowed);
                self.check_if_path_or_subpath_is_moved(location, action, place, span);
            }
            Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                let place = PlaceRef::of(place);
                self.access_place(location, place, span, Access::Read, Allowed::No);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            _ => rvalue
                .for_each_operand(&mut |operand| self.consume_operand(location, operand, span)),
        }
    }

    fn consume_operand(&mut self, location: Location, operand: &Operand, span: MirSpan) {
        let span = operand.span.unwrap_or(span);
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place)
                if self.ctx.moved_place(self.body, operand).is_none() =>
            {
                let place = PlaceRef::of(place);
                self.access_place(location, place, span, Access::Read, Allowed::No);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                let place = PlaceRef::of(place);
                self.check_movable_place(location, place, span);
                self.access_place(location, place, span, Access::Move, Allowed::Yes);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            OperandKind::Constant { .. }
            | OperandKind::Allocation { .. }
            | OperandKind::Static(_) => (),
        }
    }

    fn mutate_place(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        self.check_if_assigned_path_is_moved(location, place, span);
        // An immutable variable can be assigned while it has never been initialized.
        if place.projection.is_empty() && self.ctx.is_immutable(place.local) {
            // A binding of a pattern is a new variable each time, where rustc ends the storage
            // of the bindings of a candidate whose guard fails; lowering does not.
            let binds = matches!(span, MirSpan::PatId(_) | MirSpan::BindingId(_));
            if !binds && self.ever.contains(place.local.into_raw().into_u32() as usize) {
                self.report_illegal_reassignment(place, span);
            }
            return;
        }
        self.access_place(location, place, span, Access::Mutate, Allowed::No);
    }

    fn access_place(
        &mut self,
        _location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
        access: Access,
        allowed: Allowed,
    ) {
        let key = (place.key(), span);
        if self.access_reported.contains(&key) {
            return;
        }
        let mutability_error = self.check_access_permissions(place, span, access, allowed);
        if mutability_error {
            self.access_reported.insert(key);
        }
    }

    fn check_access_permissions(
        &mut self,
        place: PlaceRef<'_>,
        span: MirSpan,
        access: Access,
        allowed: Allowed,
    ) -> bool {
        let (error, root) = match access {
            Access::MutableBorrow { closure_capture } => {
                let allowed = if closure_capture { Allowed::Yes } else { allowed };
                match self.is_mutable(place, allowed) {
                    Ok(()) => return false,
                    Err(root) => (Access::MutableBorrow { closure_capture }, root),
                }
            }
            Access::Mutate => match self.is_mutable(place, allowed) {
                Ok(()) => return false,
                Err(root) => (Access::Mutate, root),
            },
            Access::Read | Access::Move | Access::StorageDeadOrDrop => return false,
        };
        // Only a variable that has been initialized can be said to be mutated.
        if !self.ever.contains(place.local.into_raw().into_u32() as usize)
            || root == REPORTED_BY_PARENT
        {
            return false;
        }
        let span = match self.capture_temps.get(&self.assigned_local) {
            Some(&closure) if matches!(access, Access::MutableBorrow { .. }) => closure,
            _ => span,
        };
        self.report_mutability_error(place, root, span, error);
        true
    }

    /// Whether `place` may be mutated, or the longest part of it that may not.
    fn is_mutable(&self, place: PlaceRef<'_>, allowed: Allowed) -> Result<(), usize> {
        let Some((elem, _)) = place.projection.split_last() else {
            if let Some(upvar) = self.ctx.upvar_of_local(place.local) {
                return self.is_upvar_mutable(upvar, allowed, 0);
            }
            return match (self.ctx.is_immutable(place.local), allowed) {
                (true, Allowed::No) => Err(0),
                _ => Ok(()),
            };
        };
        let len = place.projection.len();
        let base = place.base(len - 1);
        match elem {
            ProjectionElem::Deref => {
                // rustc reaches a `static mut` through a `*mut` pointer, lowering through a
                // reference.
                if base.projection.is_empty() && self.static_muts.contains(&base.local) {
                    return Ok(());
                }
                let base_ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                match base_ty.kind() {
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Not) => Err(len),
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut) => {
                        let mode = match self.upvar_field_projection(place) {
                            Some(upvar) if self.ctx.upvars[upvar].by_ref => allowed,
                            _ => Allowed::Yes,
                        };
                        self.is_mutable(base, mode)
                    }
                    TyKind::RawPtr(_, rustc_ast_ir::Mutability::Not) => Err(len),
                    TyKind::RawPtr(_, rustc_ast_ir::Mutability::Mut) => Ok(()),
                    _ => self.is_mutable(base, allowed),
                }
            }
            _ => match self.upvar_field_projection(place) {
                Some(upvar) => self.is_upvar_mutable(upvar, allowed, len),
                None => self.is_mutable(base, allowed),
            },
        }
    }

    fn is_upvar_mutable(&self, upvar: usize, allowed: Allowed, len: usize) -> Result<(), usize> {
        let upvar_data = &self.ctx.upvars[upvar];
        if !upvar_data.mutable && allowed != Allowed::Yes {
            // What is behind a reference the closure cannot mutate, rustc reports where the
            // closure borrows it.
            return Err(if upvar_data.behind_deref { REPORTED_BY_PARENT } else { len });
        }
        // Mutating what a closure captured mutates the closure: not one called by `&self`.
        match self.ctx.closure_kind {
            Some(rustc_type_ir::ClosureKind::Fn) => Err(usize::MAX),
            _ => Ok(()),
        }
    }

    /// The capture of a closure `place` is, as rustc's `is_upvar_field_projection`: `(*_1).0`
    /// or the local lowering copied it into, or what either points to for a capture by
    /// reference.
    fn upvar_field_projection(&self, place: PlaceRef<'_>) -> Option<usize> {
        let (place, by_ref) = match place.projection.split_last() {
            Some((ProjectionElem::Deref, _)) => (place.base(place.projection.len() - 1), true),
            _ => (place, false),
        };
        let upvar = if place.projection.is_empty() {
            self.ctx.upvar_of_local(place.local)?
        } else if Some(place.local) == self.ctx.closure_local {
            match place.projection {
                [ProjectionElem::Deref, ProjectionElem::Field(field)]
                | [ProjectionElem::Field(field)] => field.0 as usize,
                _ => return None,
            }
        } else {
            return None;
        };
        (!by_ref || self.ctx.upvars.get(upvar)?.by_ref).then_some(upvar)
    }

    fn report_mutability_error(
        &mut self,
        place: PlaceRef<'_>,
        root: usize,
        span: MirSpan,
        access: Access,
    ) {
        let (code, verb) = match access {
            Access::Mutate => ("E0594", "assign to"),
            _ => ("E0596", "borrow"),
        };
        let name = self.quoted(place, "data");
        let reason = if root == usize::MAX {
            match access {
                Access::Mutate => {
                    ", as `Fn` closures cannot mutate their captured variables".to_owned()
                }
                _ => ", as it is a captured variable in a `Fn` closure".to_owned(),
            }
        } else {
            let root_place = place.base(root.min(place.projection.len()));
            match root_place.projection.last() {
                Some(ProjectionElem::Deref) => {
                    let base = root_place.base(root_place.projection.len() - 1);
                    let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                    if matches!(ty.kind(), TyKind::RawPtr(..)) {
                        ", which is behind a `*const` pointer".to_owned()
                    } else if self.describe(root_place).is_some() {
                        ", which is behind a `&` reference".to_owned()
                    } else {
                        " as it is behind a `&` reference".to_owned()
                    }
                }
                _ => ", as it is not declared as mutable".to_owned(),
            }
        };
        let message = match access {
            Access::Mutate => format!("cannot {verb} {name}{reason}"),
            _ => format!("cannot {verb} {name} as mutable{reason}"),
        };
        self.report(code, span, message);
    }

    fn report_illegal_reassignment(&mut self, place: PlaceRef<'_>, span: MirSpan) {
        let name = self.describe(place).unwrap_or_default();
        let message = if self.body.param_locals.contains(&place.local) {
            format!("cannot assign to immutable argument `{name}`")
        } else {
            format!("cannot assign twice to immutable variable `{name}`")
        };
        self.report("E0384", span, message);
    }

    /// The path of `place`, or the nearest above it, with how much of `place` it covers.
    fn move_path_closest_to(&self, place: PlaceRef<'_>) -> (usize, Mpi) {
        let path = self.moves.find(self.ctx, self.body, place.local, place.projection).path();
        (self.moves.paths[path].projection.len(), path)
    }

    fn move_path_for(&self, place: PlaceRef<'_>) -> Option<Mpi> {
        match self.moves.find(self.ctx, self.body, place.local, place.projection) {
            Lookup::Exact(path) => Some(path),
            Lookup::Parent(_) => None,
        }
    }

    fn check_if_full_path_is_moved(
        &mut self,
        location: Location,
        action: InitAction,
        place: PlaceRef<'_>,
        span: MirSpan,
    ) {
        let (_, path) = self.move_path_closest_to(place);
        if self.uninit.contains(path) {
            self.report_use_of_moved_or_uninitialized(location, action, place, span, path);
        }
    }

    fn check_if_path_or_subpath_is_moved(
        &mut self,
        location: Location,
        action: InitAction,
        place: PlaceRef<'_>,
        span: MirSpan,
    ) {
        let (_, path) = self.move_path_closest_to(place);
        if self.uninit.contains(path) {
            self.report_use_of_moved_or_uninitialized(location, action, place, span, path);
            return;
        }
        if let Some(path) = self.move_path_for(place) {
            let uninit = &self.uninit;
            if let Some(moved) = self.moves.find_in_descendants(path, &|it| uninit.contains(it)) {
                self.report_use_of_moved_or_uninitialized(location, action, place, span, moved);
            }
        }
    }

    fn check_if_assigned_path_is_moved(
        &mut self,
        location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
    ) {
        for len in (0..place.projection.len()).rev() {
            let base = place.base(len);
            match place.projection[len] {
                ProjectionElem::Index(_)
                | ProjectionElem::ConstantIndex { .. }
                | ProjectionElem::Subslice { .. }
                | ProjectionElem::Downcast(_) => (),
                ProjectionElem::Deref => {
                    self.check_if_full_path_is_moved(location, InitAction::Use, base, span);
                    break;
                }
                ProjectionElem::Field(_) => {
                    let ty = self.ctx.place_ty(self.body, base.local, base.projection);
                    match ty.ty.kind() {
                        TyKind::Adt(def, _) if self.ctx.has_destructor(def.def_id()) => {
                            self.check_if_path_or_subpath_is_moved(
                                location,
                                InitAction::Assignment,
                                base,
                                span,
                            );
                            break;
                        }
                        TyKind::Adt(..) | TyKind::Tuple(..) => {
                            self.check_parent_of_field(location, base, span);
                        }
                        _ => (),
                    }
                }
            }
        }
    }

    fn check_parent_of_field(&mut self, location: Location, base: PlaceRef<'_>, span: MirSpan) {
        let mut shortest = None;
        let mut len = base.projection.len();
        loop {
            let prefix = base.base(len);
            if let Some(path) = self.move_path_for(prefix)
                && self.uninit.contains(path)
            {
                shortest = Some((prefix, path));
            }
            if len == 0 || base.projection[len - 1] == ProjectionElem::Deref {
                break;
            }
            len -= 1;
        }
        let Some((_, path)) = shortest else { return };
        let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
        if let TyKind::Adt(def, _) = ty.kind()
            && def.is_union()
            && self.moves.moves_of[path]
                .iter()
                .any(|&it| self.is_predecessor_of(self.moves.moves[it].location, location))
        {
            return;
        }
        self.report_use_of_moved_or_uninitialized(
            location,
            InitAction::PartialAssignment,
            base,
            span,
            path,
        );
    }

    fn is_predecessor_of(&self, from: Location, to: Location) -> bool {
        if from.block == to.block && from.statement < to.statement {
            return true;
        }
        let mut seen = vec![false; self.body.basic_blocks.len()];
        let mut stack: Vec<_> = self.cfg.successors[index(from.block)].to_vec();
        while let Some(block) = stack.pop() {
            if block == to.block {
                return true;
            }
            if std::mem::replace(&mut seen[index(block)], true) {
                continue;
            }
            stack.extend(self.cfg.successors[index(block)].iter().copied());
        }
        false
    }

    fn check_movable_place(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        let mut ty = crate::mir::PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        if ty.ty.references_non_lt_error() {
            return;
        }
        for (len, elem) in place.projection.iter().enumerate() {
            let kind = match (elem, ty.ty.kind()) {
                (ProjectionElem::Deref, TyKind::Ref(..) | TyKind::RawPtr(..)) => {
                    Some(MoveErrorKind::Borrowed(len))
                }
                (ProjectionElem::Field(_) | ProjectionElem::Downcast(_), TyKind::Adt(def, _))
                    if !def.is_box() && self.ctx.has_destructor(def.def_id()) =>
                {
                    Some(MoveErrorKind::Destructor(len))
                }
                (
                    ProjectionElem::Index(_)
                    | ProjectionElem::ConstantIndex { .. }
                    | ProjectionElem::Subslice { .. },
                    TyKind::Slice(_),
                ) => Some(MoveErrorKind::SliceOrArray(len)),
                (ProjectionElem::Index(_), TyKind::Array(..)) => {
                    Some(MoveErrorKind::SliceOrArray(len))
                }
                _ => None,
            };
            if let Some(kind) = kind {
                let moved = self.ctx.place_ty(self.body, place.local, place.projection).ty;
                if moved.references_non_lt_error() || self.ctx.is_copy(moved) {
                    return;
                }
                self.move_errors.push(MoveError { place: place.key(), location, span, kind });
                return;
            }
            ty = self.ctx.project(ty, elem);
            if ty.ty.references_non_lt_error() {
                return;
            }
        }
    }

    fn report_use_of_moved_or_uninitialized(
        &mut self,
        location: Location,
        action: InitAction,
        used: PlaceRef<'_>,
        span: MirSpan,
        path: Mpi,
    ) {
        let move_outs = self.moved_indexes(location, path);
        if move_outs.is_empty() {
            // rustc reports only variables: lowering leaves some temporaries unassigned, as
            // the `()` an assignment evaluates to.
            if self.ctx.local_name(used.local).is_none() || !self.uninit_reported.insert(used.local)
            {
                return;
            }
            let name = self.quoted(used, "value");
            let message = match action {
                InitAction::PartialAssignment | InitAction::Assignment => {
                    format!("partially assigned binding {name} isn't fully initialized")
                }
                _ => format!("{} binding {name} isn't initialized", action.as_general_verb()),
            };
            self.report("E0381", span, message);
            return;
        }
        if let Some((reported, _, _)) = self.move_reported.get(&move_outs) {
            let reported = PlaceRef { local: reported.0, projection: &reported.1 };
            if used.is_prefix_of(reported) {
                return;
            }
        }
        let moved = self.moves.paths[path].projection.len() < used.projection.len()
            || self.moves.paths[path].local != used.local;
        let partially = !moved && self.moves.paths[path].projection.len() > used.projection.len();
        let name = self.quoted(used, "value");
        let what = if partially { "partially moved value" } else { "moved value" };
        let message = format!("{} of {what}: {name}", action.noun());
        let diagnostic = BorrowckDiagnostic { code: "E0382", span, message };
        self.move_reported.insert(move_outs, (used.key(), location, diagnostic));
    }

    /// The moves out of `path` or a path above it that reach `location` without an init in
    /// between, as rustc's `get_moved_indexes`.
    fn moved_indexes(&self, location: Location, path: Mpi) -> Vec<usize> {
        let mut paths = vec![path];
        let mut up = self.moves.paths[path].parent;
        while let Some(parent) = up {
            paths.push(parent);
            up = self.moves.paths[parent].parent;
        }
        let mut stack = Vec::new();
        let mut back_edges = Vec::new();
        for predecessor in self.cfg.predecessor_locations(self.body, location) {
            if self.dominators.location_dominates(location, predecessor) {
                back_edges.push(predecessor);
            } else {
                stack.push(predecessor);
            }
        }
        let is_argument = self
            .body
            .param_locals
            .iter()
            .any(|&local| paths.contains(&self.moves.local_path(local)));
        let mut visited = FxHashSet::default();
        let mut result = Vec::new();
        let mut reached_start = false;
        let mut visit = |result: &mut Vec<usize>, location: Location| -> bool {
            if !visited.insert(location) {
                return true;
            }
            let is_storage_dead = matches!(
                self.body.basic_blocks[location.block].statements.get(location.statement),
                Some(statement) if matches!(statement.kind, StatementKind::StorageDead(_))
            );
            if !is_storage_dead && let Some(at) = self.moves.moves_at.get(&location) {
                for &move_out in at {
                    if paths.contains(&self.moves.moves[move_out].path) {
                        result.push(move_out);
                        return true;
                    }
                }
            }
            if let Some(at) = self.moves.inits_at.get(&location) {
                for &init in at {
                    let init = self.moves.inits[init];
                    if matches!(init.kind, InitKind::Deep | InitKind::NonPanicPathOnly)
                        && paths.contains(&init.path)
                    {
                        return true;
                    }
                }
            }
            false
        };
        while let Some(location) = stack.pop() {
            if visit(&mut result, location) {
                continue;
            }
            let mut has_predecessor = false;
            for predecessor in self.cfg.predecessor_locations(self.body, location) {
                if self.dominators.location_dominates(location, predecessor) {
                    back_edges.push(predecessor);
                } else {
                    stack.push(predecessor);
                }
                has_predecessor = true;
            }
            if !has_predecessor {
                reached_start = true;
            }
        }
        if (is_argument || !reached_start) && result.is_empty() {
            while let Some(location) = back_edges.pop() {
                if visit(&mut result, location) {
                    continue;
                }
                back_edges.extend(self.cfg.predecessor_locations(self.body, location));
            }
        }
        result.sort_unstable();
        result.dedup();
        result
    }

    /// The expression a variable's pattern matches on, for each variable a `match`, a
    /// `let` with a pattern or an `if let` binds.
    fn scrutinee_of(&mut self, binding: BindingId) -> Option<Option<ExprId>> {
        let store = self.ctx.store;
        let map = self.scrutinees.get_or_insert_with(|| {
            let mut map = FxHashMap::default();
            let mut add = |pat, scrutinee: ExprId, is_let: bool| {
                let simple = is_let && matches!(store[pat], Pat::Bind { subpat: None, .. });
                store.walk_bindings_in_pat(pat, |binding| {
                    map.insert(binding, if simple { None } else { Some(scrutinee) });
                });
            };
            for (_, expr) in store.exprs() {
                match expr {
                    hir_def::hir::Expr::Match { expr, arms } => {
                        for arm in arms.iter() {
                            add(arm.pat, *expr, false);
                        }
                    }
                    hir_def::hir::Expr::Let { pat, expr } => add(*pat, *expr, false),
                    hir_def::hir::Expr::Block { statements, .. } => {
                        for statement in statements.iter() {
                            if let Statement::Let { pat, initializer: Some(init), .. } = statement {
                                add(*pat, *init, true);
                            }
                        }
                    }
                    _ => (),
                }
            }
            map
        });
        map.get(&binding).copied()
    }

    fn report_move_errors(&mut self) {
        let errors = std::mem::take(&mut self.move_errors);
        let mut reported_groups: FxHashSet<ExprId> = FxHashSet::default();
        for error in errors {
            let mut span = error.span;
            // A variable bound by a pattern is reported where the value matched on is, once
            // for all the variables of the pattern.
            let statement = self.body.basic_blocks[error.location.block]
                .statements
                .get(error.location.statement);
            if let Some(statement) = statement
                && let StatementKind::Assign(
                    target,
                    Rvalue::Use(Operand { kind: OperandKind::Move(_), .. }),
                ) = &statement.kind
                && target.projection.as_slice().is_empty()
                && let Some(&binding) = self.ctx.bindings.get(&target.local)
            {
                match self.scrutinee_of(binding) {
                    Some(Some(scrutinee)) => {
                        if !reported_groups.insert(scrutinee) {
                            continue;
                        }
                        span = MirSpan::ExprId(scrutinee);
                    }
                    Some(None) => span = statement.span,
                    None => (),
                }
            }
            let message = self.move_error_message(&error);
            let code = match error.kind {
                MoveErrorKind::Borrowed(len) if self.is_slice_or_array(&error, len + 1) => "E0508",
                MoveErrorKind::Borrowed(_) => "E0507",
                MoveErrorKind::SliceOrArray(_) => "E0508",
                MoveErrorKind::Destructor(_) => "E0509",
            };
            self.report(code, span, message);
        }
    }

    fn is_slice_or_array(&self, error: &MoveError, len: usize) -> bool {
        let ty = self.ctx.place_ty(self.body, error.place.0, &error.place.1[..len]).ty;
        matches!(ty.kind(), TyKind::Slice(_) | TyKind::Array(..))
    }

    fn move_error_message(&self, error: &MoveError) -> String {
        let place = PlaceRef { local: error.place.0, projection: &error.place.1 };
        match error.kind {
            MoveErrorKind::Borrowed(len) if self.is_slice_or_array(error, len + 1) => {
                let ty = self.ctx.place_ty(self.body, place.local, &place.projection[..len + 1]).ty;
                let kind = if matches!(ty.kind(), TyKind::Slice(_)) { "slice" } else { "array" };
                format!("cannot move out of type `{ty:?}`, a non-copy {kind}")
            }
            MoveErrorKind::Borrowed(len) => {
                let base = place.base(len);
                let base_ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                let behind = match base_ty.kind() {
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut) => "a mutable reference",
                    TyKind::Ref(..) => "a shared reference",
                    _ => "a raw pointer",
                };
                match self.describe(place) {
                    Some(name) => format!("cannot move out of `{name}` which is behind {behind}"),
                    None => format!("cannot move out of {behind}"),
                }
            }
            MoveErrorKind::Destructor(len) => {
                let base = place.base(len);
                let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                format!("cannot move out of type `{ty:?}`, which implements the `Drop` trait")
            }
            MoveErrorKind::SliceOrArray(len) => {
                let base = place.base(len);
                let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                let kind = if matches!(ty.kind(), TyKind::Slice(_)) { "slice" } else { "array" };
                format!("cannot move out of type `{ty:?}`, a non-copy {kind}")
            }
        }
    }
}

fn capture_temps(body: &MirBody<'_>) -> FxHashMap<LocalId, MirSpan> {
    let mut temps = FxHashMap::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(
                _,
                Rvalue::Aggregate(crate::mir::AggregateKind::Closure(_), operands),
            ) = &statement.kind
            {
                for operand in operands.iter() {
                    if let OperandKind::Move(place) = &operand.kind
                        && place.projection.as_slice().is_empty()
                    {
                        temps.insert(place.local, statement.span);
                    }
                }
            }
        }
    }
    temps
}

fn static_muts(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> FxHashSet<LocalId> {
    let mut locals = FxHashSet::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(
                place,
                Rvalue::Use(Operand { kind: OperandKind::Static(id), .. }),
            ) = &statement.kind
                && place.projection.as_slice().is_empty()
                && hir_def::signatures::StaticSignature::of(ctx.db, *id)
                    .flags
                    .contains(hir_def::signatures::StaticFlags::MUTABLE)
            {
                locals.insert(place.local);
            }
        }
    }
    locals
}

fn operand_place(operand: &Operand) -> Option<PlaceRef<'_>> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => Some(PlaceRef::of(place)),
        _ => None,
    }
}
