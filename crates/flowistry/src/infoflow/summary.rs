//! Callee summaries: what a function may do to the places its callers can observe,
//! described independently of any caller.
//!
//! A summary is computed by a flow analysis of the callee body whose columns are
//! *origins* instead of locations: every leaf place of a parameter (including the
//! places behind the parameters' pointers) has an origin of its own. The rows of the
//! analysis then say which parameter places each place of the callee depends on.
//! Writes to places behind the parameters' pointers are accumulated wherever they
//! happen (including on paths that unwind), and the parts of the return place are
//! read at the normal exits. The places are finally parsed into body-independent
//! [`EffectPath`]s, which each call site translates into its own places (see
//! [`CallSite::translate`](super::callsite::CallSite::translate)).

use std::{cell::RefCell, rc::Rc};

use indexical::{
  IndexedDomain,
  bitset::rustc::{IndexMatrix, IndexSet},
};
use rustc_hir::def_id::LocalDefId;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{TyCtxt, TyKind, TypingEnv},
};
use rustc_mir_dataflow::Analysis;
use smallvec::SmallVec;

use super::{
  FlowAnalysis,
  callsite::{
    CalleeAbi, EffectPath, FallbackReason, RowRole, UnsupportedOp,
    cmp_places_structurally,
  },
  interior::Handle,
  mutation::{CalleeEffect, Mutation, Precision},
  session::AnalysisSession,
};
use crate::mir::{
  bitset::IndexSetExt,
  engine,
  placeinfo::{NormPlace, PlaceInfo},
};

/// A parameter place of a callee whose value may flow into the callee's effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Origin(usize);

indexical::define_index_type! {
  pub(crate) struct OriginIndex for Origin = u32;
}

type OriginMatrix<'tcx> = IndexMatrix<NormPlace<'tcx>, Origin>;

/// How a caller provides a parameter place that the callee may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum InputContents {
  /// The place is a reference: the callee reads its value, the address. Whatever
  /// the callee reads behind it is an origin of its own.
  Address,
  /// The place may hold pointers that the callee cannot name (e.g. a value of a
  /// generic type): the callee may read everything reachable from it.
  Reachable,
}

/// A parameter place that the callee may read, see [`Origin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SummaryInput {
  pub path: EffectPath,
  pub contents: InputContents,
}

/// Where a callee effect lands in the caller.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub(crate) enum EffectKind {
  /// (A part of) the return place, i.e. the call destination.
  Return,
  /// A place behind a pointer passed as an argument.
  ArgPointee,
  /// A shared handle passed by value (e.g. an `Rc<RefCell<T>>`), whose state was
  /// written through it (see [`Handle::Owning`]).
  SharedState,
}

impl EffectKind {
  /// The caller effect, once the precision of its translation is known.
  pub fn with(self, precision: Precision) -> CalleeEffect {
    match self {
      EffectKind::Return => CalleeEffect::Return(precision),
      EffectKind::ArgPointee => CalleeEffect::ArgPointee(precision),
      EffectKind::SharedState => CalleeEffect::SharedState(precision),
    }
  }
}

/// A write of the callee that its callers observe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SummaryEffect {
  pub kind: EffectKind,
  pub path: EffectPath,
  /// Indices into [`CalleeSummary::origins`] of the places the written value may
  /// depend on.
  pub inputs: Vec<usize>,
}

/// What a function may do to the places its callers can observe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CalleeSummary {
  /// How the callee's parameters correspond to call operands.
  pub abi: CalleeAbi,
  /// The parameter places the callee may read.
  pub origins: Vec<SummaryInput>,
  /// The origins the callee reads (possibly without writing anything).
  pub reads: Vec<usize>,
  /// The origins that the unit parts of the return value depend on (e.g. the
  /// condition deciding between `Ok(())` and `Err(())`): the dependencies of the
  /// return value as a whole.
  pub whole_return_inputs: Vec<usize>,
  /// The observable writes, parents before their children.
  pub effects: Vec<SummaryEffect>,
  /// The call operands passed to parameters of an opaque type (see
  /// [`CalleeAbi::opaque_operands`]).
  pub opaque_operands: SmallVec<[usize; 4]>,
}

/// Computes the summary of the body of `def_id`, or says why it cannot be
/// summarized.
pub(crate) fn compute<'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  def_id: LocalDefId,
) -> Result<CalleeSummary, FallbackReason> {
  let tcx = session.tcx();
  if tcx.is_coroutine(def_id.to_def_id())
    || matches!(
      tcx
        .type_of(def_id)
        .instantiate_identity()
        .skip_normalization()
        .kind(),
      TyKind::CoroutineClosure(..)
    )
  {
    return Err(FallbackReason::UnsupportedBody(UnsupportedOp::Coroutine));
  }
  let facts = session.body(def_id);
  let body = &facts.body;
  if let Some(op) = unsupported_op(tcx, body) {
    return Err(FallbackReason::UnsupportedBody(op));
  }
  let place_info =
    PlaceInfo::build_with_mode(tcx, def_id.to_def_id(), facts, session.mode());
  // Loans behind deeper pointers are ignored, so writes through them would be
  // missed.
  if place_info.arg_pointers_truncated() {
    return Err(FallbackReason::ArgPointersTruncated);
  }
  let abi = CalleeAbi::of_body(tcx, def_id.to_def_id(), body);

  // Only the leaves of the parameters get origins. Seeding a parent with the origins
  // of all its children would make siblings depend on each other.
  let mut leaves = place_info
    .all_args()
    .map(|(place, _)| place)
    .filter(|place| place_info.children(*place).len() == 1)
    .collect::<Vec<_>>();
  leaves.sort_by(|p1, p2| {
    cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
  });
  leaves.dedup();
  let (seeds, origins): (Vec<_>, Vec<_>) = leaves
    .into_iter()
    .filter_map(|place| {
      let path = match abi.classify(place_info.normalize(place)) {
        RowRole::ArgDirect(path) | RowRole::ArgPointee(path) => path,
        // Not a place of a parameter, so not something a caller provides.
        RowRole::Return(_) | RowRole::Internal => return None,
      };
      let contents = if place.ty(body.local_decls(), tcx).ty.is_ref() {
        InputContents::Address
      } else {
        InputContents::Reachable
      };
      Some((place, SummaryInput { path, contents }))
    })
    .unzip();

  let opaque_operands = abi.opaque_operands(tcx, body);
  let domain = Rc::new(IndexedDomain::from_iter((0 .. origins.len()).map(Origin)));
  let location_domain = place_info.location_domain().clone();
  let flow = FlowAnalysis::with_session(
    tcx,
    def_id.to_def_id(),
    body,
    place_info,
    session.clone(),
  );
  let analysis = SummaryAnalysis {
    flow,
    abi,
    seeds,
    writes: RefCell::new(OriginMatrix::new(&domain)),
    reads: RefCell::new(IndexSet::new(&domain)),
    domain,
  };
  log::info!(target: "flowistry::audit", "audit solve summary {}", tcx.def_path_str(def_id));
  let results =
    engine::iterate_to_fixpoint_by_location(tcx, body, location_domain, analysis);
  let analysis = &results.analysis;
  let place_info = &analysis.flow.place_info;

  // The parts of the return place at the normal exits.
  let mut returns = OriginMatrix::new(&analysis.domain);
  let mut whole_return = IndexSet::new(&analysis.domain);
  let return_leaves = place_info
    .children(Place::return_place())
    .into_iter()
    .filter(|place| place_info.children(*place).len() == 1)
    .collect::<Vec<_>>();
  for (block, data) in body.basic_blocks.iter_enumerated() {
    if !matches!(data.terminator().kind, TerminatorKind::Return) {
      continue;
    }
    let state = results.state_at(body.terminator_loc(block));
    for leaf in &return_leaves {
      let deps = analysis.flow.deps_of_inputs(&*state, &[*leaf]);
      if leaf.ty(body.local_decls(), tcx).ty.is_unit() {
        whole_return.union(&deps);
      } else {
        returns.union_into_row(place_info.normalize(*leaf), &deps);
      }
    }
  }

  let writes = analysis.writes.borrow();
  let returns = returns
    .rows()
    .filter_map(|(row, deps)| match abi.classify(*row) {
      RowRole::Return(path) => Some((EffectKind::Return, path, deps)),
      RowRole::ArgDirect(_) | RowRole::ArgPointee(_) | RowRole::Internal => None,
    });
  // Only the places behind pointer parameters and the shared handles passed by
  // value are recorded (see `SummaryAnalysis::apply`).
  let writes = writes
    .rows()
    .filter_map(|(row, deps)| match abi.classify(*row) {
      RowRole::ArgPointee(path) => Some((EffectKind::ArgPointee, path, deps)),
      RowRole::ArgDirect(path) => Some((EffectKind::SharedState, path, deps)),
      RowRole::Return(_) | RowRole::Internal => None,
    });
  let mut effects = returns
    .chain(writes)
    .map(|(kind, path, deps)| SummaryEffect {
      kind,
      path,
      inputs: origin_indices(deps),
    })
    .collect::<Vec<_>>();
  // Parents before their children, in a deterministic order.
  effects.sort_by(|e1, e2| {
    (e1.path.elems.len(), &e1.path, e1.kind).cmp(&(
      e2.path.elems.len(),
      &e2.path,
      e2.kind,
    ))
  });

  Ok(CalleeSummary {
    abi,
    origins,
    reads: origin_indices(&analysis.reads.borrow()),
    whole_return_inputs: origin_indices(&whole_return),
    effects,
    opaque_operands,
  })
}

fn origin_indices(deps: &IndexSet<Origin>) -> Vec<usize> {
  let mut indices = deps.iter().map(|origin| origin.0).collect::<Vec<_>>();
  indices.sort_unstable();
  indices
}

/// The same field-sensitive origin analysis used by callee summaries, seeded
/// only with the inputs reached by a pin. Location provenance alone cannot
/// distinguish two fields of the same incoming parameter (both start at Arg).
pub(super) struct InputFlow<'tcx> {
  results: engine::AnalysisResults<'tcx, 'tcx, SummaryAnalysis<'tcx, 'tcx>>,
}

pub(super) fn input_flow<'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  def: LocalDefId,
  seeds: Vec<Place<'tcx>>,
  shared: bool,
) -> InputFlow<'tcx> {
  let tcx = session.tcx();
  let facts = session.body(def);
  let body = &facts.body;
  let info = PlaceInfo::build_with_mode(tcx, def.to_def_id(), facts, session.mode());
  let domain = Rc::new(IndexedDomain::from_iter((0 .. seeds.len()).map(Origin)));
  let locations = info.location_domain().clone();
  let mut flow =
    FlowAnalysis::with_session(tcx, def.to_def_id(), body, info, session.clone());
  if shared {
    flow.shared_handles = super::SharedHandles::build(&flow.place_info);
    flow.hidden = super::HiddenState::build(&flow.place_info, session, true);
  }
  let analysis = SummaryAnalysis {
    flow,
    seeds,
    abi: CalleeAbi::of_body(tcx, def.to_def_id(), body),
    writes: RefCell::new(OriginMatrix::new(&domain)),
    reads: RefCell::new(IndexSet::new(&domain)),
    domain,
  };
  InputFlow {
    results: engine::iterate_to_fixpoint_by_location(tcx, body, locations, analysis),
  }
}

impl<'tcx> InputFlow<'tcx> {
  pub(super) fn reaches_before(&self, at: Location, place: Place<'tcx>) -> bool {
    let state = self.results.state_before(at);
    self
      .results
      .analysis
      .flow
      .deps_of_inputs(&state, &[place])
      .count()
      > 0
  }

  pub(super) fn spans(
    &self,
    spanner: &rustc_utils::source_map::spanner::Spanner<'tcx>,
  ) -> Vec<rustc_span::Span> {
    use rustc_utils::{
      BodyExt, SpanExt, mir::location_or_arg::LocationOrArg,
      source_map::spanner::EnclosingHirSpans,
    };
    let flow = &self.results.analysis.flow;
    let body = flow.body;
    let mut affected = Vec::new();
    for seed in &self.results.analysis.seeds {
      affected.push(LocationOrArg::Arg(seed.local));
    }
    self.results.for_each_state(|location, state| {
      let (written, mut reads): (Vec<_>, Vec<_>) = match body.stmt_at(location) {
        either::Either::Left(statement) => (
          flow
            .statement_mutations(statement, location)
            .iter()
            .map(|m| m.mutated)
            .collect(),
          vec![],
        ),
        either::Either::Right(terminator) => {
          let effects = flow.effects_at(terminator, location);
          (
            effects.mutations.iter().map(|m| m.mutated).collect(),
            effects.reads.clone(),
          )
        }
      };
      if let either::Either::Right(Terminator {
        kind: TerminatorKind::SwitchInt { discr, .. },
        ..
      }) = body.stmt_at(location)
      {
        reads.extend(discr.place());
      }
      if flow.deps_of_inputs(state, &written).count() > 0
        || flow
          .deps_of_inputs(&self.results.state_before(location), &reads)
          .count()
          > 0
      {
        affected.push(LocationOrArg::Location(location));
      }
    });
    let spans = affected
      .iter()
      .flat_map(|at| spanner.location_to_spans(*at, body, EnclosingHirSpans::OuterOnly))
      .collect();
    let mut spans = super::merge_spans(spans);
    let simple = super::simple_args::collect(flow.tcx, flow.def_id.expect_local());
    // A relevant call does not make its independent argument expressions part
    // of the forward slice, just as in ordinary value focus.
    for (block, data) in body.basic_blocks.iter_enumerated() {
      if let TerminatorKind::Call { args, .. } = &data.terminator().kind {
        for arg in args {
          let incoming = self.results.state_before(body.terminator_loc(block));
          let relevant = arg.node.place().is_some_and(|p| {
            flow
              .deps_of_inputs(&incoming, &flow.reachable_contents(p))
              .count()
              > 0
          });
          if simple.calls.contains(&arg.span) && !relevant {
            spans = spans
              .into_iter()
              .flat_map(|span| span.subtract(vec![arg.span]))
              .collect();
          }
        }
      }
      for (statement_index, statement) in data.statements.iter().enumerate() {
        let StatementKind::Assign(assignment) = &statement.kind else {
          continue;
        };
        let Rvalue::Aggregate(kind, operands) = &assignment.1 else {
          continue;
        };
        if !matches!(**kind, AggregateKind::Adt(_, _, _, _, None)) {
          continue;
        }
        let Some(fields) = simple.fields.get(&statement.source_info.span) else {
          continue;
        };
        let at = Location {
          block,
          statement_index,
        };
        let controlled = flow
          .control_dependencies
          .dependent_on(block)
          .into_iter()
          .flat_map(|blocks| blocks.iter())
          .any(|guard| {
            let TerminatorKind::SwitchInt { discr, .. } =
              &body.basic_blocks[guard].terminator().kind
            else {
              return false;
            };
            discr
              .place()
              .is_some_and(|p| self.reaches_before(body.terminator_loc(guard), p))
          });
        if controlled {
          continue;
        }
        for (index, span, simple) in fields {
          let Some(operand) = operands.get(*index) else {
            continue;
          };
          if operand.place().is_some_and(|p| self.reaches_before(at, p)) {
            continue;
          }
          if !simple
            && body.all_locations().any(|other| {
              other != at
                && span.contains(body.source_info(other).span)
                && affected.contains(&LocationOrArg::Location(other))
            })
          {
            continue;
          }
          spans = spans
            .into_iter()
            .flat_map(|outer| outer.subtract(vec![*span]))
            .collect();
        }
      }
    }
    spans
  }
}

/// The flow analysis of a callee over origins.
struct SummaryAnalysis<'a, 'tcx> {
  flow: FlowAnalysis<'a, 'tcx>,
  abi: CalleeAbi,
  /// The parameter place of each origin.
  seeds: Vec<Place<'tcx>>,
  domain: Rc<IndexedDomain<Origin>>,
  /// The places behind pointer parameters written anywhere in the body, with the
  /// origins of the written values.
  writes: RefCell<OriginMatrix<'tcx>>,
  /// The origins read anywhere in the body.
  reads: RefCell<IndexSet<Origin>>,
}

impl<'tcx> SummaryAnalysis<'_, 'tcx> {
  fn apply(
    &self,
    state: &mut OriginMatrix<'tcx>,
    mutations: &[Mutation<'tcx>],
    reads: &[Place<'tcx>],
    location: Location,
  ) {
    // Everything the instruction reads: its explicit reads, the inputs of its
    // mutations, and the conditions it depends on.
    let mut inputs = reads.to_vec();
    for mutation in mutations {
      inputs.extend_from_slice(&mutation.inputs);
    }
    let body = self.flow.body;
    for block in self
      .flow
      .control_dependencies
      .dependent_on(location.block)
      .into_iter()
      .flat_map(|blocks| blocks.iter())
    {
      if let TerminatorKind::SwitchInt { discr, .. } =
        &body.basic_blocks[block].terminator().kind
      {
        inputs.extend(discr.place());
      }
    }
    self
      .reads
      .borrow_mut()
      .union(&self.flow.deps_of_inputs(state, &inputs));

    // Locations are not origins: nothing seeds the dependencies of an instruction.
    self.flow.transfer(state, mutations, location, |_, _| {});
    let place_info = &self.flow.place_info;
    let tcx = self.flow.tcx;
    let typing_env = TypingEnv::post_analysis(tcx, self.flow.def_id);
    let mut writes = self.writes.borrow_mut();
    for mutation in mutations {
      for alias in self.flow.written_aliases(mutation.mutated) {
        let row = place_info.normalize(alias);
        let observable = match self.abi.classify(row) {
          RowRole::ArgPointee(_) => true,
          // A parameter itself is private to the callee, unless it is a shared
          // handle (e.g. an `Rc<RefCell<T>>`) passed by value: it stands for the
          // state its caller shares through it.
          RowRole::ArgDirect(_) => matches!(
            Handle::parse(tcx, typing_env, row.ty(self.flow.body, tcx)),
            Some(Handle::Owning { .. })
          ),
          RowRole::Return(_) | RowRole::Internal => false,
        };
        if observable {
          let deps = self.flow.deps_of_inputs(state, &[alias]);
          writes.union_into_row(row, &deps);
        }
      }
    }
  }
}

impl<'tcx> Analysis<'tcx> for SummaryAnalysis<'_, 'tcx> {
  type Domain = OriginMatrix<'tcx>;

  const NAME: &'static str = "CalleeSummary";

  fn bottom_value(&self, _body: &Body<'tcx>) -> Self::Domain {
    OriginMatrix::new(&self.domain)
  }

  fn initialize_start_block(&self, _body: &Body<'tcx>, state: &mut Self::Domain) {
    for (i, place) in self.seeds.iter().enumerate() {
      state.insert(self.flow.place_info.normalize(*place), Origin(i));
    }
  }

  fn apply_primary_statement_effect(
    &self,
    state: &mut Self::Domain,
    statement: &Statement<'tcx>,
    location: Location,
  ) {
    let mutations = self.flow.statement_mutations(statement, location);
    self.apply(state, &mutations, &[], location);
  }

  fn apply_primary_terminator_effect<'mir>(
    &self,
    state: &mut Self::Domain,
    terminator: &'mir Terminator<'tcx>,
    location: Location,
  ) -> TerminatorEdges<'mir, 'tcx> {
    let effects = self.flow.effects_at(terminator, location);
    self.apply(state, &effects.mutations, &effects.reads, location);
    terminator.edges()
  }

  fn apply_call_return_effect(
    &self,
    _state: &mut Self::Domain,
    _block: BasicBlock,
    _return_places: CallReturnPlaces<'_, 'tcx>,
  ) {
  }
}

/// The first operation of `body` through which it may access its parameters without
/// the analysis knowing, if any.
fn unsupported_op<'tcx>(tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> Option<UnsupportedOp> {
  // Raw pointers carry no loans, so a write through one (e.g. handed to an opaque
  // callee: `ptr::write(&raw mut self.b, ..)`) would not reach any parameter.
  let holds_raw_pointer = body.local_decls.iter().any(|decl| {
    decl
      .ty
      .walk()
      .any(|arg| arg.as_type().is_some_and(|ty| ty.is_raw_ptr()))
  });
  if holds_raw_pointer {
    return Some(UnsupportedOp::RawPointer);
  }
  let mut visitor = UnsupportedOps {
    tcx,
    body,
    found: None,
  };
  visitor.visit_body(body);
  visitor.found
}

struct UnsupportedOps<'a, 'tcx> {
  tcx: TyCtxt<'tcx>,
  body: &'a Body<'tcx>,
  found: Option<UnsupportedOp>,
}

impl<'tcx> Visitor<'tcx> for UnsupportedOps<'_, 'tcx> {
  fn visit_place(
    &mut self,
    place: &Place<'tcx>,
    context: visit::PlaceContext,
    location: Location,
  ) {
    for (base, elem) in place.iter_projections() {
      let base_ty = base.ty(self.body.local_decls(), self.tcx).ty;
      if matches!(elem, ProjectionElem::Field(..))
        && base_ty.ty_adt_def().is_some_and(|adt| adt.is_union())
      {
        self.found.get_or_insert(UnsupportedOp::Union);
      }
    }
    self.super_place(place, context, location);
  }

  fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: Location) {
    if let Rvalue::Cast(CastKind::Transmute, ..) = rvalue {
      self.found.get_or_insert(UnsupportedOp::Transmute);
    }
    self.super_rvalue(rvalue, location);
  }

  fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, location: Location) {
    let unsupported = match &terminator.kind {
      TerminatorKind::Call { func, .. } => func
        .const_fn_def()
        .is_some_and(|(def_id, _)| {
          self.tcx.intrinsic(def_id).is_some_and(|intrinsic| {
            matches!(intrinsic.name.as_str(), "transmute" | "transmute_unchecked")
          })
        })
        .then_some(UnsupportedOp::Transmute),
      TerminatorKind::InlineAsm { .. } => Some(UnsupportedOp::InlineAsm),
      TerminatorKind::Yield { .. } | TerminatorKind::CoroutineDrop => {
        Some(UnsupportedOp::Coroutine)
      }
      TerminatorKind::TailCall { .. } => Some(UnsupportedOp::TailCall),
      TerminatorKind::Goto { .. }
      | TerminatorKind::SwitchInt { .. }
      | TerminatorKind::UnwindResume
      | TerminatorKind::UnwindTerminate(_)
      | TerminatorKind::Return
      | TerminatorKind::Unreachable
      | TerminatorKind::Drop { .. }
      | TerminatorKind::Assert { .. }
      | TerminatorKind::FalseEdge { .. }
      | TerminatorKind::FalseUnwind { .. } => None,
    };
    if let Some(op) = unsupported {
      self.found.get_or_insert(op);
    }
    self.super_terminator(terminator, location);
  }
}
