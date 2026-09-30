use std::{cell::{Cell, RefCell}, rc::Rc};

use indexical::{
  IndexedValue,
  bitset::rustc::{IndexMatrix, IndexSet},
};
use log::{debug, trace};
use rustc_data_structures::fx::FxHashMap as HashMap;
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{TyCtxt, TypingEnv},
};
use rustc_mir_dataflow::Analysis;
use rustc_utils::{
  BodyExt, OperandExt, PlaceExt,
  mir::{
    control_dependencies::ControlDependencies,
    location_or_arg::{
      LocationOrArg,
      index::{LocationOrArgDomain, LocationOrArgSet},
    },
  },
};
use smallvec::SmallVec;

use super::{
  AnalysisSession,
  effects::CallEffects,
  mutation::{ModularMutationVisitor, Mutation, MutationStatus},
  shared_handles::SharedHandles,
};
use crate::{
  extensions::{ContextMode, MutabilityMode},
  mir::{
    placeinfo::{NormPlace, PlaceInfo},
    utils::ErasedTy,
  },
};

/// Represents the information flows at a given instruction. See [`FlowResults`](super::FlowResults) for a high-level explanation of this datatype.
///
/// `FlowDomain` represents $\Theta$ that maps from places $p$ to dependencies $\kappa$. To efficiently represent $\kappa$, a set of locations,
/// we use the bit-set data structures in [`rustc_index::bit_set`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_index/bit_set/index.html).
/// However instead of using a bit-set directly, we use the [`indexical`] crate to map between raw indices and the objects they represent.
///
/// The [`IndexMatrix`] maps from a [`NormPlace`] to a [`LocationOrArgSet`] via the [`IndexMatrix::row_set`] method. Rows are keyed by
/// normalized places (see [`PlaceInfo::normalize`]), never by raw [`Place`]s: use [`PlaceInfo::normalize`] to compute the key of a place.
/// The [`LocationOrArgSet`] is an
/// [`IndexSet`](indexical::IndexSet) of locations (or arguments, see note below), which wraps a
/// [`rustc_index::bit_set::HybridBitSet`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_index/bit_set/enum.HybridBitSet.html) and
/// has roughly the same API. The [`indexical`] crate has a concept of an [`IndexedDomain`](indexical::IndexedDomain) to represent the mapping from
/// a set of values to the indexes those values --- [`LocationOrArgDomain`] is the implementation for locations.
///
/// # **Note:** reading dependencies from `FlowDomain`
/// In general, you should *not* use [`FlowDomain::row_set`] directly. This is because the `FlowDomain` does not have exactly the same structure as
/// the $\Theta$ described in the paper. Based on performance profiling, we have determined that the size of the `FlowDomain` is the primary factor that
/// increases Flowistry's memory usage and runtime. So we generally trade-off making `FlowDomain` smaller in exchange for making dependency lookups more
/// computationally expensive.
///
/// Instead, you should use [`FlowAnalysis::deps_for`](crate::infoflow::FlowAnalysis::deps_for) to read a place's dependencies out of a given `FlowDomain`.
///
/// # **Note:** arguments as dependencies
/// Because function arguments are never initialized, there is no "root" location for argument places. This fact poses a problem for
/// information flow analysis: an instruction `bb[0]: _2 = _1` (where `_1` is an argument) would set $\Theta(\verb|_2|) = \Theta(\verb|_1|) \cup \\{\verb|bb0\[0\]|\\}\$.
/// However, $\Theta(\verb|_1|)$ would be empty, so it would be imposible to determine that `_2` depends on `_1`. To solve this issue, we
/// enrich the domain of locations with arguments, using the [`LocationOrArg`] type. Any dependency can be on *either* a location or an argument.
pub type FlowDomain<'tcx> = IndexMatrix<NormPlace<'tcx>, LocationOrArg>;

/// Data structure that holds context for performing the information flow analysis.
pub struct FlowAnalysis<'a, 'tcx> {
  /// The type context used for the analysis.
  pub tcx: TyCtxt<'tcx>,

  /// The ID of the body being analyzed.
  pub def_id: DefId,

  /// The body being analyzed.
  pub body: &'a Body<'tcx>,

  /// The metadata about places used in the analysis.
  pub place_info: PlaceInfo<'a, 'tcx>,

  pub(crate) counters: TransferCounters,
  pub(crate) control_dependencies: ControlDependencies<BasicBlock>,

  /// The session providing callee summaries, with the same mode as `place_info`.
  pub(crate) session: Rc<AnalysisSession<'tcx>>,

  /// The effects of each terminator in `Recurse` mode (see
  /// [`FlowAnalysis::effects_at`]).
  pub(crate) call_effects: RefCell<HashMap<Location, Rc<CallEffects<'tcx>>>>,

  /// The dependencies of what each terminator reads (see [`CallEffects::reads`]),
  /// before its mutations, in `Recurse` mode.
  pub(crate) call_reads: RefCell<HashMap<Location, LocationOrArgSet>>,

  /// In the pessimistic analysis of
  /// [`compute_flow_with_shared_handles`](super::compute_flow_with_shared_handles),
  /// the handles that may share state.
  pub(crate) shared_handles: Option<SharedHandles<'tcx>>,
}

/// Counters of the transfer function, see [`FlowStats`](super::FlowStats).
#[derive(Default)]
pub(crate) struct TransferCounters {
  pub(crate) transfers: Cell<usize>,
  pub(crate) mutations: Cell<usize>,
}

impl<'a, 'tcx> FlowAnalysis<'a, 'tcx> {
  /// Constructs (but does not execute) a new FlowAnalysis, with a new
  /// [`AnalysisSession`] in the mode of `place_info`.
  pub fn new(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    body: &'a Body<'tcx>,
    place_info: PlaceInfo<'a, 'tcx>,
  ) -> Self {
    let session = AnalysisSession::new(tcx, place_info.mode());
    Self::with_session(tcx, def_id, body, place_info, session)
  }

  /// Constructs a new FlowAnalysis that shares the callee summaries of `session`,
  /// whose mode must be the mode of `place_info`.
  pub(crate) fn with_session(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    body: &'a Body<'tcx>,
    place_info: PlaceInfo<'a, 'tcx>,
    session: Rc<AnalysisSession<'tcx>>,
  ) -> Self {
    assert_eq!(session.mode(), place_info.mode());
    let control_dependencies = body.control_dependencies();
    debug!("Control dependencies: {control_dependencies:?}");
    FlowAnalysis {
      tcx,
      def_id,
      body,
      place_info,
      control_dependencies,
      counters: TransferCounters::default(),
      session,
      call_effects: RefCell::default(),
      call_reads: RefCell::default(),
      shared_handles: None,
    }
  }

  /// Returns the [`LocationOrArgDomain`] used by the analysis.
  pub fn location_domain(&self) -> &Rc<LocationOrArgDomain> {
    self.place_info.location_domain()
  }

  fn influences(&self, place: Place<'tcx>) -> SmallVec<[Place<'tcx>; 8]> {
    let conflicts = self
      .place_info
      .aliases(place)
      .iter()
      .flat_map(|alias| self.place_info.conflicts(*alias));
    let provenance =
      place
        .refs_in_projection(self.body, self.tcx)
        .flat_map(|(place_ref, _)| {
          self
            .place_info
            .aliases(Place::from_ref(place_ref, self.tcx))
            .iter()
        });
    conflicts.chain(provenance).copied().collect()
  }

  /// Returns all the dependencies of `place` within `state`.
  ///
  /// Prefer using this method instead of accessing `FlowDomain` directly,
  /// unless you *really* know what you're doing.
  pub fn deps_for(
    &self,
    state: &FlowDomain<'tcx>,
    place: Place<'tcx>,
  ) -> LocationOrArgSet {
    let mut deps = LocationOrArgSet::new(self.location_domain());
    for subplace in self
      .place_info
      .reachable_values(place, Mutability::Not)
      .iter()
      .flat_map(|place| self.influences(*place))
    {
      deps.union(state.row_set(&self.place_info.normalize(subplace)));
    }
    deps
  }

  /// The union of the rows of `state` that influence each of `inputs`.
  pub(crate) fn deps_of_inputs<C: IndexedValue + 'static>(
    &self,
    state: &IndexMatrix<NormPlace<'tcx>, C>,
    inputs: &[Place<'tcx>],
  ) -> IndexSet<C> {
    let mut deps = IndexSet::new(state.col_domain());
    for input in inputs {
      for relevant in self.influences(*input) {
        deps.union(state.row_set(&self.place_info.normalize(relevant)));
      }
    }
    deps
  }

  /// The places written by a mutation of `mutated`: its aliases, except those behind
  /// a shared reference (unless mutability is ignored, or the alias has interior
  /// mutability).
  pub(crate) fn written_aliases(
    &self,
    mutated: Place<'tcx>,
  ) -> SmallVec<[Place<'tcx>; 8]> {
    let ignore_mut = match self.place_info.mode().mutability_mode {
      MutabilityMode::IgnoreMut => true,
      MutabilityMode::DistinguishMut => false,
    };
    let typing_env = TypingEnv::post_analysis(self.tcx, self.def_id);
    self
      .place_info
      .aliases(mutated)
      .iter()
      .filter(|alias| {
        // Remove any conflicts that aren't actually mutable, e.g. if x : &T ends up
        // as an alias of y: &mut T. See test function_lifetime_alias_mut for an example.
        let has_immut = alias.iter_projections().any(|(sub_place, _)| {
          let ty = sub_place.ty(self.body.local_decls(), self.tcx).ty;
          matches!(ty.ref_mutability(), Some(Mutability::Not))
        });
        // State behind a shared reference can still be written if it is interior
        // mutable, e.g. a `RefCell` written through a guard obtained from `&self`.
        let interior_mutable = || {
          let ty = alias.ty(self.body.local_decls(), self.tcx).ty;
          !ErasedTy::new(self.tcx, ty).is_freeze(self.tcx, typing_env)
        };
        !has_immut || ignore_mut || interior_mutable()
      })
      .copied()
      .collect()
  }

  // This function expects *ALL* the mutations that occur within a given [`Location`] at once.
  pub(crate) fn transfer_function(
    &self,
    state: &mut FlowDomain<'tcx>,
    mutations: &[Mutation<'tcx>],
    location: Location,
  ) {
    self.transfer(state, mutations, location, |location, deps| {
      deps.insert(location);
    });
  }

  /// Applies the mutations of `location` to a state whose rows are sets of `C`.
  ///
  /// `seed` adds what an instruction contributes to the dependencies of the values
  /// it writes by itself: its location, in a [`FlowDomain`]. It is applied to the
  /// location of the mutations and to the terminators they are control-dependent on.
  ///
  /// This function expects *all* the mutations of a location at once.
  pub(crate) fn transfer<C: IndexedValue + std::fmt::Debug + 'static>(
    &self,
    state: &mut IndexMatrix<NormPlace<'tcx>, C>,
    mutations: &[Mutation<'tcx>],
    location: Location,
    seed: impl Fn(Location, &mut IndexSet<C>),
  ) {
    debug!("  Applying mutations {mutations:?}");
    let counters = &self.counters;
    counters.transfers.set(counters.transfers.get() + 1);
    counters.mutations.set(counters.mutations.get() + mutations.len());

    // Initialize dependencies to include current location of mutation.
    let mut all_deps = {
      let mut deps = IndexSet::new(state.col_domain());
      seed(location, &mut deps);
      vec![deps; mutations.len()]
    };

    // Add every influence on `input` to `deps`.
    let add_deps =
      |state: &IndexMatrix<NormPlace<'tcx>, C>, input, target_deps: &mut IndexSet<C>| {
        for relevant in self.influences(input) {
          let relevant_deps = state.row_set(&self.place_info.normalize(relevant));
          trace!("    For relevant {relevant:?} for input {input:?}");
          target_deps.union(relevant_deps);
        }
      };

    // Register every explicitly provided input as an input.
    for (mt, deps) in mutations.iter().zip(&mut all_deps) {
      for input in &mt.inputs {
        add_deps(state, *input, deps);
      }
    }

    // Add location of every control dependency.
    let controlled_by = self.control_dependencies.dependent_on(location.block);
    let body = self.body;
    for block in controlled_by.into_iter().flat_map(|set| set.iter()) {
      for deps in &mut all_deps {
        seed(body.terminator_loc(block), deps);
      }

      // Include dependencies of the switch's operand.
      let terminator = body.basic_blocks[block].terminator();
      if let TerminatorKind::SwitchInt { discr, .. } = &terminator.kind
        && let Some(discr_place) = discr.as_place()
      {
        for deps in &mut all_deps {
          add_deps(state, discr_place, deps);
        }
      }
    }

    for (mt, deps) in mutations.iter().zip(&mut all_deps) {
      // Clear sub-places of mutated place (if sound to do so)
      if mt.status() == MutationStatus::Definitely
        && self.place_info.aliases(mt.mutated).len() == 1
      {
        for sub in self.place_info.children(mt.mutated).iter() {
          state.clear_row(&self.place_info.normalize(*sub));
        }
      }

      // Add deps of mutated to include provenance of mutated pointers
      add_deps(state, mt.mutated, deps);

      let mutable_aliases = self.written_aliases(mt.mutated);

      debug!("  Mutated places: {mutable_aliases:?}");
      debug!("    with deps {deps:?}");

      for alias in &mutable_aliases {
        state.union_into_row(self.place_info.normalize(*alias), deps);
      }

      // Pessimistic analysis only: other handles to the same interior-mutable
      // state may point to the object that was just written.
      if let Some(handles) = &self.shared_handles {
        let shared = mutable_aliases
          .iter()
          .flat_map(|alias| handles.possibly_shared(mt, *alias, &self.place_info))
          .collect::<SmallVec<[_; 8]>>();
        for row in shared {
          state.union_into_row(row, deps);
        }
      }
    }
  }

  fn recurse(&self) -> bool {
    match self.place_info.mode().context_mode {
      ContextMode::Recurse => true,
      ContextMode::SigOnly => false,
    }
  }
}

impl<'a, 'tcx> Analysis<'tcx> for FlowAnalysis<'a, 'tcx> {
  type Domain = FlowDomain<'tcx>;

  const NAME: &'static str = "FlowAnalysis";

  fn bottom_value(&self, _body: &Body<'tcx>) -> Self::Domain {
    FlowDomain::new(self.location_domain())
  }

  fn initialize_start_block(&self, _body: &Body<'tcx>, state: &mut Self::Domain) {
    for (arg, loc) in self.place_info.all_args() {
      for place in self.place_info.conflicts(arg) {
        debug!(
          "arg={arg:?} / place={place:?} / loc={:?}",
          self.location_domain().value(loc)
        );
        state.insert(self.place_info.normalize(*place), loc);
      }
    }
  }

  fn apply_primary_statement_effect(
    &self,
    state: &mut Self::Domain,
    statement: &Statement<'tcx>,
    location: Location,
  ) {
    let mutations = self.statement_mutations(statement, location);
    if mutations.is_empty() {
      return;
    }
    debug_assert!(definite_writes_disjoint(&mutations), "{mutations:?}");
    self.transfer_function(state, &mutations, location);
  }

  fn apply_primary_terminator_effect<'mir>(
    &self,
    state: &mut Self::Domain,
    terminator: &'mir Terminator<'tcx>,
    location: Location,
  ) -> TerminatorEdges<'mir, 'tcx> {
    if self.recurse() {
      let effects = self.effects_at(terminator, location);
      // What the terminator reads may not flow into any of its mutations (e.g. a
      // call returning `()`). Record it before the mutations change it.
      let reads = self.deps_of_inputs(state, &effects.reads);
      self
        .call_reads
        .borrow_mut()
        .entry(location)
        .and_modify(|prior| {
          prior.union(&reads);
        })
        .or_insert(reads);
      self.transfer_function(state, &effects.mutations, location);
    } else {
      ModularMutationVisitor::new(&self.place_info, |_, mutations| {
        debug_assert!(definite_writes_disjoint(&mutations), "{mutations:?}");
        self.transfer_function(state, &mutations, location)
      })
      .visit_terminator(terminator, location);
    }

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

/// Whether no two definite writes of a batch overlap.
///
/// `transfer_function` applies a batch sequentially, and a definite write clears the
/// place it writes, so overlapping definite writes would depend on their order. The
/// modular approximation never produces them (callee effects are ordered instead).
fn definite_writes_disjoint<'tcx>(mutations: &[Mutation<'tcx>]) -> bool {
  let definite = mutations
    .iter()
    .filter(|mt| mt.status() == MutationStatus::Definitely)
    .map(|mt| mt.mutated)
    .collect::<SmallVec<[_; 8]>>();
  let is_prefix = |a: &Place<'tcx>, b: &Place<'tcx>| {
    a.local == b.local && b.projection.starts_with(a.projection)
  };
  definite.iter().enumerate().all(|(i, a)| {
    definite[i + 1 ..]
      .iter()
      .all(|b| !is_prefix(a, b) && !is_prefix(b, a))
  })
}
