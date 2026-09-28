//! Field-origin summaries, independent of caller source locations.

use std::{cell::RefCell, rc::Rc};

use indexical::{
  IndexedDomain,
  bitset::rustc::{IndexMatrix, IndexSet},
};
use rustc_data_structures::fx::FxHashSet;
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::TyCtxt,
};
use rustc_mir_dataflow::Analysis;
use rustc_utils::{OperandExt, PlaceExt};

use super::{
  FlowAnalysis, effects::CallEffects, mutation::Mutation, session::AnalysisSession,
};
use crate::mir::{engine, placeinfo::PlaceInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Origin(usize);
indexical::define_index_type! { struct OriginIndex for Origin = u32; }
type Domain<'tcx> = IndexMatrix<Place<'tcx>, Origin>;

pub(crate) struct Effect<'tcx> {
  pub place: Place<'tcx>,
  pub inputs: Vec<Place<'tcx>>,
}

pub(crate) struct CalleeSummary<'tcx> {
  pub reads: Vec<Place<'tcx>>,
  pub writes: Vec<Effect<'tcx>>,
}

struct SummaryAnalysis<'a, 'tcx> {
  flow: FlowAnalysis<'a, 'tcx>,
  places: Vec<Place<'tcx>>,
  domain: Rc<IndexedDomain<Origin>>,
  reads: RefCell<IndexSet<Origin>>,
  // Accumulate effects at *every* write, not just at normal returns. Rows also
  // record constant writes with an empty set of input origins.
  writes: RefCell<Domain<'tcx>>,
}

impl<'tcx> SummaryAnalysis<'_, 'tcx> {
  fn apply(
    &self,
    state: &mut Domain<'tcx>,
    mutations: &[Mutation<'tcx>],
    reads: &[Place<'tcx>],
    location: Location,
  ) {
    let mut inputs = reads.to_vec();
    for mt in mutations {
      inputs.extend_from_slice(&mt.inputs);
    }
    if let Some(blocks) = self.flow.control_dependencies.dependent_on(location.block) {
      for block in blocks.iter() {
        if let TerminatorKind::SwitchInt { discr, .. } =
          &self.flow.body.basic_blocks[block].terminator().kind
        {
          inputs.extend(discr.as_place());
        }
      }
    }
    self
      .reads
      .borrow_mut()
      .union(&self.flow.inputs_deps(state, &inputs));
    self.flow.transfer(state, mutations, location, |_, _| {});
    for mutation in mutations {
      for alias in self.flow.place_info.aliases(mutation.mutated) {
        if alias.is_arg(self.flow.body) && alias.is_indirect() {
          let deps = self.flow.inputs_deps(state, &[*alias]);
          self.writes.borrow_mut().union_into_row(*alias, &deps);
        }
      }
    }
  }
}

impl<'tcx> Analysis<'tcx> for SummaryAnalysis<'_, 'tcx> {
  type Domain = Domain<'tcx>;
  const NAME: &'static str = "CalleeSummary";
  fn bottom_value(&self, _: &Body<'tcx>) -> Self::Domain {
    Domain::new(&self.domain)
  }
  fn initialize_start_block(&self, _: &Body<'tcx>, state: &mut Self::Domain) {
    for (i, place) in self.places.iter().enumerate() {
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
    let effects: Rc<CallEffects<'tcx>> = self.flow.effects_at(terminator, location);
    self.apply(state, &effects.mutations, &effects.reads, location);
    terminator.edges()
  }
  fn apply_call_return_effect(
    &self,
    _: &mut Self::Domain,
    _: BasicBlock,
    _: CallReturnPlaces<'_, 'tcx>,
  ) {
  }
}

// Refining a body with memory operations outside the alias model would turn
// missing effects into a false claim of independence. Keep the signature there.
struct Unsupported<'a, 'tcx> {
  body: &'a Body<'tcx>,
  tcx: TyCtxt<'tcx>,
  found: bool,
}
impl<'tcx> Visitor<'tcx> for Unsupported<'_, 'tcx> {
  fn visit_place(
    &mut self,
    place: &Place<'tcx>,
    context: rustc_middle::mir::visit::PlaceContext,
    location: Location,
  ) {
    for (base, _) in place.iter_projections() {
      let ty = base.ty(self.body, self.tcx).ty;
      if ty.is_raw_ptr() || ty.ty_adt_def().is_some_and(|adt| adt.is_union()) {
        self.found = true;
      }
    }
    self.super_place(place, context, location);
  }
  fn visit_terminator(&mut self, term: &Terminator<'tcx>, location: Location) {
    if !matches!(
      term.kind,
      TerminatorKind::Goto { .. }
        | TerminatorKind::SwitchInt { .. }
        | TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::UnwindResume
        | TerminatorKind::UnwindTerminate(..)
        | TerminatorKind::Call { .. }
        | TerminatorKind::Drop { .. }
        | TerminatorKind::Assert { .. }
        | TerminatorKind::FalseEdge { .. }
        | TerminatorKind::FalseUnwind { .. }
    ) {
      self.found = true;
    }
    self.super_terminator(term, location);
  }
}

pub(crate) fn compute<'tcx>(
  session: Rc<AnalysisSession<'tcx>>,
  tcx: TyCtxt<'tcx>,
  def: DefId,
) -> Option<CalleeSummary<'tcx>> {
  let facts = session.body(def);
  let body = &facts.body;
  let mut unsupported = Unsupported {
    body,
    tcx,
    found: false,
  };
  unsupported.visit_body(body);
  // Raw pointers carry no loans, so a write through one handed to an opaque
  // callee (ptr::write(&raw mut self.b, ..)) would not reach any argument.
  let holds_raw_pointer = body.local_decls.iter().any(|decl| {
    decl
      .ty
      .walk()
      .any(|arg| arg.as_type().is_some_and(|ty| ty.is_raw_ptr()))
  });
  if unsupported.found || holds_raw_pointer {
    session.fallback("unsupported MIR operation");
    return None;
  }
  // Aliases::compute_loans and PlaceInfo::all_args deliberately bound incoming
  // pointer paths to two projections. Do not claim independence based on absent
  // origins for deeper borrowed contents; use the caller's signature fallback.
  if body.args_iter().any(|arg| {
    Place::from(arg)
      .interior_pointers(tcx, body, def)
      .into_values()
      .flatten()
      .any(|(pointer, _)| pointer.projection.len() > 2)
  }) {
    session.fallback("argument pointer depth exceeds alias model");
    return None;
  }
  let place_info = PlaceInfo::build(tcx, def, facts);
  // Only leaf fields get input origins. Seeding ancestor rows with all their
  // children's origins would immediately make siblings dependent again.
  let places = place_info
    .all_args()
    .map(|(p, _)| p)
    .filter(|p| place_info.children(*p).len() == 1)
    .collect::<FxHashSet<_>>()
    .into_iter()
    .collect::<Vec<_>>();
  let domain = Rc::new(IndexedDomain::from_iter((0 .. places.len()).map(Origin)));
  let location_domain = place_info.location_domain().clone();
  let flow = FlowAnalysis::with_session(tcx, def, body, place_info, session);
  let analysis = SummaryAnalysis {
    flow,
    places,
    reads: RefCell::new(IndexSet::new(&domain)),
    writes: RefCell::new(Domain::new(&domain)),
    domain,
  };
  let results = engine::iterate_to_fixpoint(tcx, body, location_domain, analysis);
  let analysis = &results.analysis;
  let mut writes = analysis.writes.borrow().clone();
  for (bb, data) in body.basic_blocks.iter_enumerated() {
    if matches!(data.terminator().kind, TerminatorKind::Return) {
      let state = results.state_at(body.terminator_loc(bb));
      for field in analysis.flow.place_info.children(Place::from(RETURN_PLACE)) {
        if analysis.flow.place_info.children(field).len() == 1
          && !field.ty(body, tcx).ty.is_unit()
        {
          let deps = analysis.flow.inputs_deps(state, &[field]);
          writes.union_into_row(field, &deps);
        }
      }
    }
  }
  let expand = |deps: &IndexSet<Origin>| {
    deps
      .iter()
      .map(|origin| analysis.places[origin.0])
      .collect()
  };
  Some(CalleeSummary {
    reads: expand(&analysis.reads.borrow()),
    writes: writes
      .rows()
      .map(|(place, deps)| Effect {
        place: *place,
        inputs: expand(deps),
      })
      .collect(),
  })
}
