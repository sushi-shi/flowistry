//! State that no place of a body names, kept in rows of its own (*hidden cells*):
//! the statics a body or its callees access, and, in the pessimistic analysis of
//! [`compute_flow_with_shared_handles`](super::compute_flow_with_shared_handles),
//! the outside world and the memory that raw pointers reach.
//!
//! Two handles to the same static are the same memory, so the cell of a static is
//! part of the exact analysis. A place derived from a constant address of a static
//! (or from a `thread_local!` key) is a *static handle*. A write through a static
//! handle, or by a callee that may write the static, writes the cell of the static;
//! a read through a static handle, or by a callee that reads the static, reads it.
//! The callees' statics are found by scanning their bodies (see [`GlobalEffects`]),
//! not by their flow analysis, so a callee that mentions a static both reads it and,
//! if the static can change, writes it.
//!
//! The pessimistic analysis adds two cells that may conflate unrelated state:
//! - [`HiddenCell::World`], the state that FFI and the standard library's
//!   filesystem, environment, network, process and standard stream functions share
//!   through the operating system;
//! - [`HiddenCell::Escaped`], memory whose address left the ownership model. A place
//!   whose address is taken as a raw pointer (by `&raw`, by casting or transmuting a
//!   reference, or by a call returning a raw pointer from references) is *exposed*.
//!   A write through a raw pointer, or by a call whose operands reach a raw pointer,
//!   writes the cell and may write every exposed place; a direct write to an
//!   exposed place writes the cell; a read through a raw pointer, or by such a call,
//!   reads it.
//!
//! A cell is a row keyed by a local past the locals of the body (see
//! [`NormPlace::hidden`]): the transfer function reads and writes it directly, and
//! never makes a place query on it.

use std::{cell::RefCell, rc::Rc};

use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexSet};
use rustc_hir::{
  def::DefKind,
  def_id::{DefId, LocalDefId},
};
use rustc_middle::{
  mir::{
    interpret::{GlobalAlloc, Scalar},
    visit::Visitor,
    *,
  },
  ty::{Instance, InstanceKind, Ty, TyCtxt, TyKind, TypingEnv},
};
use rustc_span::Spanned;
use smallvec::SmallVec;

use super::{interior::InteriorMutation, mutation::Mutation, session::AnalysisSession};
use crate::{
  extensions::ContextMode,
  mir::{
    placeinfo::{NormPlace, PlaceInfo},
    utils::ErasedTy,
  },
};

/// State that no place of a body names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HiddenCell<'tcx> {
  /// A static, or the thread-local behind a `thread_local!` key (a `const`).
  Static(DefId),
  /// The state shared through the operating system (pessimistic analysis only).
  World,
  /// Memory reached through raw pointers (pessimistic analysis only).
  Escaped,
  /// The messages in flight in the standard library's channels of a message type
  /// (pessimistic analysis only): their ends share them through raw pointers.
  Channel(ErasedTy<'tcx>),
}

/// The statics, and in the pessimistic analysis the other cells, that a function
/// may access, including through its callees.
#[derive(Debug, Default)]
pub(crate) struct GlobalEffects {
  /// The statics it mentions.
  pub reads: FxIndexSet<DefId>,
  /// The statics it mentions that can change (see [`static_can_change`]).
  pub writes: FxIndexSet<DefId>,
  /// Whether it may access [`HiddenCell::World`].
  pub world: bool,
  /// Whether it may access [`HiddenCell::Escaped`].
  pub escaped: bool,
}

impl GlobalEffects {
  fn merge(&mut self, other: &GlobalEffects) {
    self.reads.extend(other.reads.iter().copied());
    self.writes.extend(other.writes.iter().copied());
    self.world |= other.world;
    self.escaped |= other.escaped;
  }
}

/// The global effects of the body of `def_id`, with those of its callees (see
/// [`AnalysisSession::global_effects`]).
pub(crate) fn body_global_effects<'tcx>(
  session: &AnalysisSession<'tcx>,
  def_id: LocalDefId,
) -> GlobalEffects {
  let tcx = session.tcx();
  let body = &session.body(def_id).body;
  let mut effects = GlobalEffects::default();
  let mut scan = BodyScan {
    tcx,
    body,
    effects: &mut effects,
  };
  scan.visit_body(body);
  for data in body.basic_blocks.iter() {
    if let TerminatorKind::Call { func, args, .. } = &data.terminator().kind {
      let call = call_global_effects(session, def_id.to_def_id(), body, func, args);
      effects.merge(&call);
    }
  }
  effects
}

/// Collects the statics and raw-pointer accesses of a body.
struct BodyScan<'a, 'tcx> {
  tcx: TyCtxt<'tcx>,
  body: &'a Body<'tcx>,
  effects: &'a mut GlobalEffects,
}

impl<'tcx> Visitor<'tcx> for BodyScan<'_, 'tcx> {
  fn visit_const_operand(&mut self, constant: &ConstOperand<'tcx>, location: Location) {
    if let Some(def_id) = const_static(self.tcx, constant) {
      self.effects.reads.insert(def_id);
      if static_can_change(self.tcx, def_id) {
        self.effects.writes.insert(def_id);
      }
    }
    self.super_const_operand(constant, location);
  }

  fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: Location) {
    if let Rvalue::ThreadLocalRef(def_id) = rvalue {
      self.effects.reads.insert(*def_id);
      self.effects.writes.insert(*def_id);
    }
    self.super_rvalue(rvalue, location);
  }

  fn visit_place(
    &mut self,
    place: &Place<'tcx>,
    context: visit::PlaceContext,
    location: Location,
  ) {
    if through_raw_pointer(self.tcx, self.body, *place, &FxHashSet::default()) {
      self.effects.escaped = true;
    }
    self.super_place(place, context, location);
  }
}

/// The global effects of a call of `func` with operands `args` from `caller`: those
/// of the function it resolves to, and of the closures and functions it is given.
///
/// Bodies of the source code are scanned in `Recurse` mode only: `SigOnly` mode
/// analyzes calls from their signatures and borrow-checks no other body, so it only
/// knows the effects of foreign and standard-library functions.
fn call_global_effects<'tcx>(
  session: &AnalysisSession<'tcx>,
  caller: DefId,
  body: &Body<'tcx>,
  func: &Operand<'tcx>,
  args: &[Spanned<Operand<'tcx>>],
) -> GlobalEffects {
  let tcx = session.tcx();
  let recurse = session.mode().context_mode == ContextMode::Recurse;
  let mut effects = GlobalEffects::default();
  if let Some((def_id, fn_args)) = func.const_fn_def() {
    if tcx.is_foreign_item(def_id) {
      effects.world = true;
    } else {
      let typing_env = TypingEnv::post_analysis(tcx, caller);
      let fn_args = tcx.erase_and_anonymize_regions(fn_args);
      if let Ok(Some(instance)) = Instance::try_resolve(tcx, typing_env, def_id, fn_args)
      {
        let resolved = instance.def.def_id();
        if let InstanceKind::Item(_) = instance.def
          && let Some(local) = local_fn(tcx, resolved)
        {
          if recurse {
            effects.merge(&session.global_effects(local));
          }
        } else if reaches_world(tcx, resolved, instance) {
          effects.world = true;
        }
      }
    }
  }
  // A callee may call the closures and functions it is given (e.g. `LocalKey::with`
  // or `thread::spawn`).
  for arg in args {
    let ty = arg.node.ty(body.local_decls(), tcx);
    let ty = ty.peel_refs();
    if recurse
      && let TyKind::Closure(def_id, _) | TyKind::FnDef(def_id, _) = ty.kind()
      && let Some(local) = local_fn(tcx, *def_id)
    {
      effects.merge(&session.global_effects(local));
    }
  }
  effects
}

/// `def_id`, if it is a function or closure of the local crate with a body.
fn local_fn(tcx: TyCtxt<'_>, def_id: DefId) -> Option<LocalDefId> {
  let local = def_id.as_local()?;
  matches!(
    tcx.def_kind(def_id),
    DefKind::Fn | DefKind::AssocFn | DefKind::Closure
  )
  .then_some(local)
  .filter(|_| tcx.hir_maybe_body_owned_by(local).is_some())
}

/// Whether the standard-library function `def_id` (as `instance`) accesses state
/// shared through the operating system: files, the environment, the network,
/// processes, or the standard streams.
fn reaches_world<'tcx>(
  tcx: TyCtxt<'tcx>,
  def_id: DefId,
  instance: Instance<'tcx>,
) -> bool {
  if !matches!(tcx.crate_name(def_id.krate).as_str(), "std") {
    return false;
  }
  const OS: [&str; 5] = [
    "std::fs::",
    "std::env::",
    "std::net::",
    "std::process::",
    "std::os::",
  ];
  let path = tcx.def_path_str(def_id);
  if OS.iter().any(|prefix| path.starts_with(prefix)) {
    return true;
  }
  if !path.starts_with("std::io::") {
    return false;
  }
  if matches!(path.as_str(), "std::io::stdin" | "std::io::read_to_string") {
    return true;
  }
  // A `Read`/`Write` method reaches the outside world through an OS-backed value.
  instance.args.types().any(|ty| {
    ty.walk().filter_map(|arg| arg.as_type()).any(|ty| {
      let TyKind::Adt(adt_def, _) = ty.kind() else {
        return false;
      };
      let path = tcx.def_path_str(adt_def.did());
      OS.iter().any(|prefix| path.starts_with(prefix))
        || matches!(path.as_str(), "std::io::Stdin" | "std::io::StdinLock")
    })
  })
}

/// The static (or `thread_local!` key) whose address `constant` is, if any.
pub(crate) fn const_static<'tcx>(
  tcx: TyCtxt<'tcx>,
  constant: &ConstOperand<'tcx>,
) -> Option<DefId> {
  match constant.const_ {
    Const::Val(ConstValue::Scalar(Scalar::Ptr(ptr, _)), _) => {
      match tcx.try_get_global_alloc(ptr.provenance.alloc_id())? {
        GlobalAlloc::Static(def_id) => Some(def_id),
        _ => None,
      }
    }
    Const::Unevaluated(unevaluated, ty) if is_local_key(tcx, ty) => {
      match unevaluated.promoted {
        None => Some(unevaluated.def),
        // A promoted `&KEY`: find the key in the promoted body.
        Some(promoted) => {
          let body = &tcx.promoted_mir(unevaluated.def)[promoted];
          let mut keys = KeyScan { tcx, key: None };
          keys.visit_body(body);
          keys.key
        }
      }
    }
    _ => None,
  }
}

/// Finds a `thread_local!` key constant in a promoted body.
struct KeyScan<'tcx> {
  tcx: TyCtxt<'tcx>,
  key: Option<DefId>,
}

impl<'tcx> Visitor<'tcx> for KeyScan<'tcx> {
  fn visit_const_operand(&mut self, constant: &ConstOperand<'tcx>, _: Location) {
    if self.key.is_none()
      && let Const::Unevaluated(unevaluated, ty) = constant.const_
      && unevaluated.promoted.is_none()
      && is_local_key(self.tcx, ty)
    {
      self.key = Some(unevaluated.def);
    }
  }
}

/// Whether `ty` is (a reference to) a `thread_local!` key.
fn is_local_key<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
  match ty.peel_refs().kind() {
    TyKind::Adt(adt_def, _) => {
      tcx.crate_name(adt_def.did().krate).as_str() == "std"
        && tcx.item_name(adt_def.did()).as_str() == "LocalKey"
    }
    _ => false,
  }
}

/// Whether the state behind the static `def_id` can change: it is a `static mut`,
/// has interior mutability, or is a thread-local.
fn static_can_change(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
  match tcx.def_kind(def_id) {
    DefKind::Static {
      mutability: Mutability::Mut,
      ..
    } => true,
    DefKind::Static { .. } => {
      let ty = tcx
        .type_of(def_id)
        .instantiate_identity()
        .skip_normalization();
      let typing_env = TypingEnv::post_analysis(tcx, def_id);
      !ErasedTy::new(tcx, ty).is_freeze(tcx, typing_env)
    }
    // A `thread_local!` key.
    DefKind::Const { .. } => true,
    _ => false,
  }
}

/// Whether `place` is behind a raw pointer, or behind a reference made from one
/// (a local of `raw_derived`).
fn through_raw_pointer<'tcx>(
  tcx: TyCtxt<'tcx>,
  body: &Body<'tcx>,
  place: Place<'tcx>,
  raw_derived: &FxHashSet<Local>,
) -> bool {
  place.iter_projections().any(|(base, elem)| {
    matches!(elem, ProjectionElem::Deref)
      && (base.ty(body.local_decls(), tcx).ty.is_raw_ptr()
        || (base.projection.is_empty() && raw_derived.contains(&base.local)))
  })
}

/// The reads and writes of hidden cells by the instruction at a location.
#[derive(Debug, Default)]
pub(crate) struct HiddenEffects<'tcx> {
  /// The cells read: every write of the instruction depends on them.
  pub reads: SmallVec<[NormPlace<'tcx>; 2]>,
  /// The cells written, from the inputs of the instruction.
  pub writes: SmallVec<[NormPlace<'tcx>; 2]>,
  /// The places the instruction reads besides the inputs of its mutations (the
  /// operands of a call), which the written cells depend on.
  pub operands: Vec<Place<'tcx>>,
  /// Whether the exposed places may be written too (see [`HiddenCell::Escaped`]).
  pub writes_exposed: bool,
}

impl HiddenEffects<'_> {
  pub fn is_empty(&self) -> bool {
    self.reads.is_empty() && self.writes.is_empty()
  }
}

/// The hidden cells of a body, and what each of its instructions does to them.
pub(crate) struct HiddenState<'tcx> {
  /// The statics of each static handle local.
  statics: FxHashMap<Local, Statics>,
  /// Whether some static handles are results of calls (pessimistic analysis only).
  statics_through_calls: bool,
  /// The exposed places and raw-derived locals, in the pessimistic analysis.
  pessimistic: Option<Exposure<'tcx>>,
  /// The cells used so far, each keyed by the local `first_cell + index`.
  cells: RefCell<FxIndexSet<HiddenCell<'tcx>>>,
  first_cell: usize,
  effects: RefCell<FxHashMap<Location, Rc<HiddenEffects<'tcx>>>>,
}

/// What the pessimistic analysis knows about raw pointers in a body.
struct Exposure<'tcx> {
  /// The rows of the exposed places: each place whose address is taken as a raw
  /// pointer, widened to the whole object behind its last dereference (pointer
  /// arithmetic may reach its siblings).
  rows: Rc<[NormPlace<'tcx>]>,
  /// The exposed places themselves.
  places: Vec<Place<'tcx>>,
  /// The locals holding a reference made from a raw pointer.
  raw_derived: FxHashSet<Local>,
}

impl<'tcx> HiddenState<'tcx> {
  /// The hidden cells of the body of `place_info`, with the cells of the pessimistic
  /// analysis if `pessimistic`.
  pub fn build(
    place_info: &PlaceInfo<'_, 'tcx>,
    session: &AnalysisSession<'tcx>,
    pessimistic: bool,
  ) -> Self {
    let body = place_info.body;
    let (statics, statics_through_calls) =
      static_handles(place_info, session, pessimistic);
    HiddenState {
      statics,
      statics_through_calls,
      pessimistic: pessimistic.then(|| Exposure::build(place_info)),
      cells: RefCell::default(),
      first_cell: body.local_decls.len(),
      effects: RefCell::default(),
    }
  }

  /// Whether these are the cells of the pessimistic analysis.
  #[cfg(feature = "engine-diff")]
  pub fn is_pessimistic(&self) -> bool {
    self.pessimistic.is_some()
  }

  /// The row of `cell`.
  fn row(&self, cell: HiddenCell<'tcx>) -> NormPlace<'tcx> {
    let (index, _) = self.cells.borrow_mut().insert_full(cell);
    NormPlace::hidden(Local::from_usize(self.first_cell + index))
  }

  /// The rows of the exposed places, in the pessimistic analysis.
  pub fn exposed_rows(&self) -> &[NormPlace<'tcx>] {
    self
      .pessimistic
      .as_ref()
      .map_or(&[], |exposure| &exposure.rows)
  }

  /// The effects on hidden cells of the instruction at `location`, whose mutations
  /// are `mutations` (cached).
  pub fn effects_at(
    &self,
    place_info: &PlaceInfo<'_, 'tcx>,
    session: &AnalysisSession<'tcx>,
    location: Location,
    mutations: &[Mutation<'tcx>],
  ) -> Rc<HiddenEffects<'tcx>> {
    if let Some(effects) = self.effects.borrow().get(&location) {
      return Rc::clone(effects);
    }
    let effects = Rc::new(self.compute_effects(place_info, session, location, mutations));
    self
      .effects
      .borrow_mut()
      .insert(location, Rc::clone(&effects));
    effects
  }

  fn compute_effects(
    &self,
    place_info: &PlaceInfo<'_, 'tcx>,
    session: &AnalysisSession<'tcx>,
    location: Location,
    mutations: &[Mutation<'tcx>],
  ) -> HiddenEffects<'tcx> {
    let tcx = place_info.tcx;
    let body = place_info.body;
    let mut reads = FxIndexSet::default();
    let mut writes = FxIndexSet::default();
    let mut operands = Vec::new();

    let static_of = |place: Place<'tcx>| -> &[DefId] {
      self
        .statics
        .get(&place.local)
        .map_or(&[], |statics| statics)
    };
    for mutation in mutations {
      if mutation.mutated.projection.first() == Some(&ProjectionElem::Deref) {
        writes.extend(
          static_of(mutation.mutated)
            .iter()
            .map(|s| HiddenCell::Static(*s)),
        );
      }
      for input in &mutation.inputs {
        reads.extend(static_of(*input).iter().map(|s| HiddenCell::Static(*s)));
      }
    }

    if let Some(exposure) = &self.pessimistic {
      for mutation in mutations {
        if through_raw_pointer(tcx, body, mutation.mutated, &exposure.raw_derived)
          || exposure.places.iter().any(|exposed| {
            exposed.local == mutation.mutated.local
              && (exposed.projection.starts_with(mutation.mutated.projection)
                || mutation.mutated.projection.starts_with(exposed.projection))
          })
        {
          writes.insert(HiddenCell::Escaped);
        }
        if mutation
          .inputs
          .iter()
          .any(|input| through_raw_pointer(tcx, body, *input, &exposure.raw_derived))
        {
          reads.insert(HiddenCell::Escaped);
        }
      }
    }

    if let either::Either::Right(terminator) = body.stmt_at(location)
      && let TerminatorKind::Call { func, args, .. } = &terminator.kind
    {
      operands = args
        .iter()
        .filter_map(|arg| arg.node.place())
        .collect::<Vec<_>>();
      let caller = place_info.def_id;
      let interior = InteriorMutation::of_call(tcx, caller, func);
      for arg in args {
        let statics = match &arg.node {
          Operand::Constant(constant) => {
            const_static(tcx, constant).into_iter().collect()
          }
          Operand::Copy(place) | Operand::Move(place) => Statics::from(static_of(*place)),
          Operand::RuntimeChecks(_) => Statics::new(),
        };
        for def_id in statics {
          reads.insert(HiddenCell::Static(def_id));
          if interior == InteriorMutation::Possible && static_can_change(tcx, def_id) {
            writes.insert(HiddenCell::Static(def_id));
          }
        }
      }
      let global = call_global_effects(session, caller, body, func, args);
      reads.extend(
        global
          .reads
          .iter()
          .map(|def_id| HiddenCell::Static(*def_id)),
      );
      writes.extend(
        global
          .writes
          .iter()
          .map(|def_id| HiddenCell::Static(*def_id)),
      );
      if let Some(exposure) = &self.pessimistic {
        if global.world {
          reads.insert(HiddenCell::World);
          writes.insert(HiddenCell::World);
        }
        // An operand holding a raw pointer, or a reference made from one, whose
        // pointee the callee may access.
        let raw_operand = args.iter().any(|arg| {
          reaches_raw_pointer(tcx, arg.node.ty(body.local_decls(), tcx), 0)
            || arg
              .node
              .place()
              .is_some_and(|place| exposure.raw_derived.contains(&place.local))
        });
        if global.escaped || raw_operand {
          reads.insert(HiddenCell::Escaped);
          writes.insert(HiddenCell::Escaped);
        }
        for arg in args {
          if let Some(message) =
            channel_message(tcx, arg.node.ty(body.local_decls(), tcx))
          {
            reads.insert(HiddenCell::Channel(message));
            writes.insert(HiddenCell::Channel(message));
          }
        }
      }
    }

    let writes_exposed = writes.contains(&HiddenCell::Escaped);
    HiddenEffects {
      reads: reads.into_iter().map(|cell| self.row(cell)).collect(),
      writes: writes.into_iter().map(|cell| self.row(cell)).collect(),
      operands,
      writes_exposed,
    }
  }

  /// Whether the pessimistic analysis of the body would differ from the exact one
  /// because of hidden cells.
  pub fn has_pessimistic_effects(
    &self,
    place_info: &PlaceInfo<'_, 'tcx>,
    session: &AnalysisSession<'tcx>,
  ) -> bool {
    let Some(exposure) = &self.pessimistic else {
      return false;
    };
    if self.statics_through_calls
      || !exposure.places.is_empty()
      || !exposure.raw_derived.is_empty()
    {
      return true;
    }
    let tcx = place_info.tcx;
    let body = place_info.body;
    let mut raw = RawScan {
      tcx,
      body,
      found: false,
    };
    raw.visit_body(body);
    raw.found
      || body.basic_blocks.iter().any(|data| {
        let TerminatorKind::Call { func, args, .. } = &data.terminator().kind else {
          return false;
        };
        let global = call_global_effects(session, place_info.def_id, body, func, args);
        global.world
          || global.escaped
          || args.iter().any(|arg| {
            channel_message(tcx, arg.node.ty(body.local_decls(), tcx)).is_some()
          })
          || args
            .iter()
            .any(|arg| reaches_raw_pointer(tcx, arg.node.ty(body.local_decls(), tcx), 0))
      })
  }
}

/// Finds a place behind a raw pointer.
struct RawScan<'a, 'tcx> {
  tcx: TyCtxt<'tcx>,
  body: &'a Body<'tcx>,
  found: bool,
}

impl<'tcx> Visitor<'tcx> for RawScan<'_, 'tcx> {
  fn visit_place(
    &mut self,
    place: &Place<'tcx>,
    context: visit::PlaceContext,
    location: Location,
  ) {
    self.found |= through_raw_pointer(self.tcx, self.body, *place, &FxHashSet::default());
    self.super_place(place, context, location);
  }
}

/// The statics a static handle may point to.
type Statics = SmallVec<[DefId; 1]>;

/// The static handle locals of the body of `place_info`, with their statics: the
/// locals assigned a constant address of a static (or a `thread_local!` key), and
/// the locals assigned a copy, cast or borrow of a place based on one.
///
/// In the pessimistic analysis, the result of a call that carries an address (e.g. a
/// lock guard, or the result of `Deref`) is also a handle to the statics of the
/// handles it is given, or else to the statics its callee may write.
fn static_handles<'tcx>(
  place_info: &PlaceInfo<'_, 'tcx>,
  session: &AnalysisSession<'tcx>,
  pessimistic: bool,
) -> (FxHashMap<Local, Statics>, bool) {
  let tcx = place_info.tcx;
  let body = place_info.body;
  let mut handles = FxHashMap::<Local, Statics>::default();
  let mut through_calls = false;
  loop {
    let before = handles.len();
    for data in body.basic_blocks.iter() {
      for statement in &data.statements {
        let StatementKind::Assign(assign) = &statement.kind else {
          continue;
        };
        let (place, rvalue) = &**assign;
        if !place.projection.is_empty() || handles.contains_key(&place.local) {
          continue;
        }
        let from_place = |source: &Place<'tcx>| handles.get(&source.local).cloned();
        let statics = match rvalue {
          Rvalue::Use(Operand::Constant(constant)) => {
            const_static(tcx, constant).map(|def_id| Statics::from_elem(def_id, 1))
          }
          Rvalue::ThreadLocalRef(def_id) => Some(Statics::from_elem(*def_id, 1)),
          Rvalue::Use(Operand::Copy(source) | Operand::Move(source))
          | Rvalue::Cast(_, Operand::Copy(source) | Operand::Move(source), _)
          | Rvalue::Ref(_, _, source)
          | Rvalue::RawPtr(_, source) => from_place(source),
          _ => None,
        };
        if let Some(statics) = statics {
          handles.insert(place.local, statics);
        }
      }
      if !pessimistic {
        continue;
      }
      let TerminatorKind::Call {
        func,
        args,
        destination,
        ..
      } = &data.terminator().kind
      else {
        continue;
      };
      if !destination.projection.is_empty()
        || handles.contains_key(&destination.local)
        || !carries_address(destination.ty(body.local_decls(), tcx).ty)
      {
        continue;
      }
      let mut statics = args
        .iter()
        .filter_map(|arg| arg.node.place())
        .filter_map(|place| handles.get(&place.local))
        .flatten()
        .copied()
        .collect::<Statics>();
      if statics.is_empty() {
        let global = call_global_effects(session, place_info.def_id, body, func, args);
        statics.extend(global.writes.iter().copied());
      }
      statics.sort_by_key(|def_id| (def_id.krate, def_id.index));
      statics.dedup();
      if !statics.is_empty() {
        handles.insert(destination.local, statics);
        through_calls = true;
      }
    }
    if handles.len() == before {
      return (handles, through_calls);
    }
  }
}

/// Whether a value of type `ty` may hold an address: a reference, a raw pointer, or
/// a value with a lifetime.
fn carries_address(ty: Ty<'_>) -> bool {
  ty.walk().any(|arg| {
    arg.as_region().is_some() || arg.as_type().is_some_and(|ty| ty.is_raw_ptr())
  })
}

/// The message type of `ty`, if it is (a reference to) an end of a channel of the
/// standard library.
fn channel_message<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Option<ErasedTy<'tcx>> {
  let TyKind::Adt(adt_def, args) = ty.peel_refs().kind() else {
    return None;
  };
  let did = adt_def.did();
  if tcx.crate_name(did.krate).as_str() != "std"
    || !matches!(
      tcx.item_name(did).as_str(),
      "Sender" | "SyncSender" | "Receiver"
    )
  {
    return None;
  }
  let path = tcx.def_path_str(did);
  (path.contains("::mpsc::") || path.contains("::mpmc::"))
    .then(|| ErasedTy::new(tcx, args.type_at(0)))
}

impl<'tcx> Exposure<'tcx> {
  fn build(place_info: &PlaceInfo<'_, 'tcx>) -> Self {
    let tcx = place_info.tcx;
    let body = place_info.body;
    let ty_of = |place: Place<'tcx>| place.ty(body.local_decls(), tcx).ty;
    let mut places = Vec::new();
    let mut raw_derived = FxHashSet::default();
    for data in body.basic_blocks.iter() {
      for statement in &data.statements {
        let StatementKind::Assign(assign) = &statement.kind else {
          continue;
        };
        let (place, rvalue) = &**assign;
        match rvalue {
          Rvalue::RawPtr(_, source) => places.push(*source),
          // A reference cast or transmuted to a raw pointer or an integer.
          Rvalue::Cast(_, Operand::Copy(source) | Operand::Move(source), target)
            if ty_of(*source).is_ref() && !target.is_ref() =>
          {
            places.push(tcx.mk_place_deref(*source))
          }
          // A reborrow of a raw pointer's pointee.
          Rvalue::Ref(_, _, source)
            if place.projection.is_empty()
              && through_raw_pointer(tcx, body, *source, &raw_derived) =>
          {
            raw_derived.insert(place.local);
          }
          _ => {}
        }
      }
      // A call returning a raw pointer may derive it from the references it is
      // given; one returning a reference from raw pointers returns raw memory.
      if let TerminatorKind::Call {
        args, destination, ..
      } = &data.terminator().kind
      {
        let returned = ty_of(*destination);
        let operand_tys = args
          .iter()
          .map(|arg| arg.node.ty(body.local_decls(), tcx))
          .collect::<Vec<_>>();
        if reaches_raw_pointer(tcx, returned, 0) {
          for (arg, ty) in args.iter().zip(&operand_tys) {
            if ty.is_ref()
              && let Some(source) = arg.node.place()
            {
              places.push(tcx.mk_place_deref(source));
            }
          }
        }
        if returned.is_ref()
          && destination.projection.is_empty()
          && operand_tys
            .iter()
            .any(|ty| reaches_raw_pointer(tcx, *ty, 0))
        {
          raw_derived.insert(destination.local);
        }
      }
    }
    // A place exposed through a reference is the place the reference points to, and
    // pointer arithmetic may reach any part of the object behind the last
    // dereference.
    let mut places = places
      .into_iter()
      .flat_map(|place| {
        let mut aliases = place_info
          .aliases(place)
          .iter()
          .copied()
          .collect::<Vec<_>>();
        aliases.push(place);
        aliases
      })
      .map(|place| {
        let object = place
          .projection
          .iter()
          .rposition(|elem| matches!(elem, ProjectionElem::Deref))
          .map_or(0, |i| i + 1);
        Place {
          local: place.local,
          projection: tcx.mk_place_elems(&place.projection[.. object]),
        }
      })
      .collect::<Vec<_>>();
    places.sort_by(|p1, p2| {
      super::callsite::cmp_places_structurally(
        p1.local,
        p1.projection,
        p2.local,
        p2.projection,
      )
    });
    places.dedup();
    let rows = places
      .iter()
      .map(|place| place_info.normalize(*place))
      .collect();
    Exposure {
      rows,
      places,
      raw_derived,
    }
  }
}

/// Whether a value of type `ty` holds a raw pointer that the source code manages:
/// a raw pointer, `NonNull` or `AtomicPtr`, directly or in the fields of a type of
/// the local crate, a tuple or an array (not the owning pointers inside the
/// standard library's collections, which are modelled as their contents).
fn reaches_raw_pointer<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, depth: usize) -> bool {
  if depth > 4 {
    return false;
  }
  match ty.kind() {
    TyKind::RawPtr(..) => true,
    TyKind::Ref(_, pointee, _) => reaches_raw_pointer(tcx, *pointee, depth + 1),
    TyKind::Tuple(tys) => tys.iter().any(|ty| reaches_raw_pointer(tcx, ty, depth + 1)),
    TyKind::Array(ty, _) | TyKind::Slice(ty) => reaches_raw_pointer(tcx, *ty, depth + 1),
    TyKind::Adt(adt_def, args) => {
      let did = adt_def.did();
      if matches!(tcx.crate_name(did.krate).as_str(), "core" | "alloc" | "std") {
        return matches!(tcx.item_name(did).as_str(), "NonNull" | "AtomicPtr");
      }
      did.is_local()
        && adt_def
          .all_fields()
          .any(|field| reaches_raw_pointer(tcx, field.ty(tcx, args), depth + 1))
    }
    _ => false,
  }
}
