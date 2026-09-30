use std::{
  cell::{Cell, RefCell},
  iter,
  rc::Rc,
};

use either::Either;
use indexical::{IndexedValue, bitset::rustc::IndexSet};
use log::debug;
use rustc_data_structures::fx::{FxHashMap as HashMap, FxHashSet as HashSet};
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{TyCtxt, TypingEnv},
};
use rustc_mir_dataflow::Analysis;
use rustc_utils::{
  BodyExt, OperandExt, PlaceExt,
  cache::Cache,
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
  domain::{LazyMatrix, RowMatrix, SeedRows},
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
/// The [`LazyMatrix`] maps from a [`NormPlace`] to a [`LocationOrArgSet`] via the [`LazyMatrix::row_set`] method. Rows are keyed by
/// normalized places (see [`PlaceInfo::normalize`]), never by raw [`Place`]s: use [`PlaceInfo::normalize`] to compute the key of a place.
/// The rows of argument places, which start out depending on their argument, are stored once per body and are implicit in every
/// state until they are written (see [`LazyMatrix`]); [`LazyMatrix::rows`] lists them too.
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
pub type FlowDomain<'tcx> = LazyMatrix<NormPlace<'tcx>, LocationOrArg>;

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

  /// Counters of the transfer function.
  pub(crate) counters: TransferCounters,
  pub(crate) caches: TransferCaches<'tcx>,
  /// The rows of argument places at the start of the body, shared by every state.
  pub(crate) seeds: Rc<SeedRows<NormPlace<'tcx>, LocationOrArg>>,
}

/// Counters of the transfer function, see [`FlowStats`](super::FlowStats).
#[derive(Default)]
pub(crate) struct TransferCounters {
  pub(crate) transfers: Cell<usize>,
  pub(crate) mutations: Cell<usize>,
  pub(crate) unstable_locations: Cell<usize>,
}

/// Memoized computations of the transfer function. Each depends only on its key (a
/// place or a location of the body), so it is computed at its first use, with the
/// same queries in the same order as without the cache, and reused afterwards.
#[derive(Default)]
pub(crate) struct TransferCaches<'tcx> {
  /// The batches of mutations of each location per the modular approximation.
  modular_mutations: RefCell<HashMap<Location, Rc<[Vec<Mutation<'tcx>>]>>>,
  /// The mutations of each statement (see [`FlowAnalysis::statement_mutations`]).
  pub(crate) statement_mutations: RefCell<HashMap<Location, Rc<[Mutation<'tcx>]>>>,
  /// The rows whose values influence a place (see [`FlowAnalysis::influences`]).
  influence_keys: Cache<Place<'tcx>, Box<[NormPlace<'tcx>]>>,
  /// The rows of the children of a place, which a strong update of it clears.
  children_keys: Cache<Place<'tcx>, Box<[NormPlace<'tcx>]>>,
  /// The rows of the aliases of a place that a mutation of it writes (see
  /// [`FlowAnalysis::written_aliases`]).
  written_alias_keys: Cache<Place<'tcx>, Box<[NormPlace<'tcx>]>>,
}

/// The distinct normalized places of `places`.
fn distinct_keys<'tcx>(
  place_info: &PlaceInfo<'_, 'tcx>,
  places: impl IntoIterator<Item = Place<'tcx>>,
) -> Box<[NormPlace<'tcx>]> {
  let mut seen = HashSet::default();
  places
    .into_iter()
    .map(|place| place_info.normalize(place))
    .filter(|key| seen.insert(*key))
    .collect()
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
    // Every place that conflicts with a place reachable from an argument starts out
    // depending on the argument.
    let seeds = SeedRows::new(
      place_info.location_domain(),
      place_info.all_args().flat_map(|(arg, loc)| {
        let place_info = &place_info;
        place_info
          .compute_conflicts(arg)
          .into_iter()
          .map(move |place| (place_info.normalize(place), loc))
      }),
    );
    FlowAnalysis {
      tcx,
      def_id,
      body,
      place_info,
      control_dependencies,
      session,
      call_effects: RefCell::default(),
      call_reads: RefCell::default(),
      shared_handles: None,
      counters: TransferCounters::default(),
      caches: TransferCaches::default(),
      seeds: Rc::new(seeds),
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

  /// The distinct rows of the places that influence `place` (cached).
  pub(crate) fn influence_keys(&self, place: Place<'tcx>) -> &[NormPlace<'tcx>] {
    self.caches.influence_keys.get(&place, |place| {
      distinct_keys(&self.place_info, self.influences(place))
    })
  }

  /// The distinct rows of the children of `place` (cached).
  fn children_keys(&self, place: Place<'tcx>) -> &[NormPlace<'tcx>] {
    self.caches.children_keys.get(&place, |place| {
      distinct_keys(&self.place_info, self.place_info.children(place))
    })
  }

  /// The distinct rows of the aliases of `place` that a mutation of `place` writes
  /// (cached, see [`written_aliases`](Self::written_aliases)).
  pub(crate) fn written_alias_keys(&self, place: Place<'tcx>) -> &[NormPlace<'tcx>] {
    self.caches.written_alias_keys.get(&place, |place| {
      let written = self.written_aliases(place);
      debug!("  Mutable aliases of {place:?}: {written:?}");
      distinct_keys(&self.place_info, written)
    })
  }

  /// The batches of mutations at `location` per the modular approximation (cached).
  pub(crate) fn modular_mutations(
    &self,
    location: Location,
  ) -> Rc<[Vec<Mutation<'tcx>>]> {
    if let Some(batches) = self.caches.modular_mutations.borrow().get(&location) {
      return Rc::clone(batches);
    }
    let mut batches = Vec::new();
    ModularMutationVisitor::new(&self.place_info, |_, mutations| {
      batches.push(mutations);
    })
    .visit_location(self.body, location);
    let batches = Rc::<[_]>::from(batches);
    self
      .caches
      .modular_mutations
      .borrow_mut()
      .insert(location, Rc::clone(&batches));
    batches
  }

  /// The number of reachable locations whose effect is not idempotent on its own
  /// output (see [`batch_is_idempotent`](Self::batch_is_idempotent)). The block engine
  /// computes the same states as the location engine if there are none.
  pub(crate) fn unstable_locations(&self) -> usize {
    let body = self.body;
    traversal::reverse_postorder(body)
      .flat_map(|(block, data)| {
        (0 ..= data.statements.len()).map(move |statement_index| Location {
          block,
          statement_index,
        })
      })
      .filter(|location| !self.effect_is_idempotent(*location))
      .count()
  }

  /// Whether the effect `f` of `location` satisfies `f(x ∨ f(x)) = f(x)` for every
  /// state `x`, i.e. whether applying it again to its own output adds nothing. See
  /// [`batch_is_idempotent`](Self::batch_is_idempotent).
  fn effect_is_idempotent(&self, location: Location) -> bool {
    match self.body.stmt_at(location) {
      Either::Left(statement) => {
        let mutations = self.statement_mutations(statement, location);
        self.batch_is_idempotent(&mutations, location)
      }
      Either::Right(terminator) if self.recurse() => {
        let effects = self.effects_at(terminator, location);
        self.batch_is_idempotent(&effects.mutations, location)
          && self.reads_are_stable(&effects)
      }
      Either::Right(_) => match &*self.modular_mutations(location) {
        [] => true,
        [mutations] => self.batch_is_idempotent(mutations, location),
        // Batches are applied one after the other.
        _ => false,
      },
    }
  }

  /// Whether the dependencies of what a terminator reads (recorded in
  /// [`call_reads`](Self::call_reads) from its pre-state) are the same in `x` and in
  /// `x ∨ f(x)` for every state `x`, where `f` is the effect of the terminator.
  ///
  /// The location engine applies the effect of a revisited location to its own
  /// previous output joined with its predecessors' states, so it records the reads of
  /// `x ∨ f(x)`; the block engine records those of `x`. They are the same if the
  /// terminator writes none of the rows it reads: a written row gains the location of
  /// the terminator itself, which need not be among the dependencies of the reads.
  fn reads_are_stable(&self, effects: &CallEffects<'tcx>) -> bool {
    if effects.reads.is_empty() || effects.mutations.is_empty() {
      return true;
    }
    let read = effects
      .reads
      .iter()
      .flat_map(|place| self.influence_keys(*place))
      .copied()
      .collect::<HashSet<_>>();
    effects.mutations.iter().all(|mt| {
      self
        .written_alias_keys(mt.mutated)
        .iter()
        .copied()
        .chain(self.possibly_shared_rows(mt))
        .all(|key| !read.contains(&key))
    })
  }

  /// Whether the transfer function `f` of a batch of mutations satisfies
  /// `f(x ∨ f(x)) = f(x)` for every state `x`.
  ///
  /// `f` computes the dependencies `D_i` of each mutation `i` from the rows of its
  /// inputs and of the control dependencies in `x` (the *pre-state*), then applies the
  /// mutations in order: each clears the children of its place if it is a strong
  /// update, adds the rows of the provenance of its place in the current state to
  /// `D_i`, and adds `D_i` to the rows of the aliases of its place. So `D_i` is the
  /// union of the values in `x` of a set of *source* rows `S_i` (plus locations that
  /// do not depend on `x`): the rows it reads from `x` (its *base* reads) and the
  /// sources of the writers of the provenance rows it reads.
  ///
  /// `x ∨ f(x)` differs from `x` only in rows `w` that `f` writes, whose value becomes
  /// that of the sources `T_w` of `f(x)(w)` (`w` itself unless a mutation clears it, and
  /// the sources of its writers after the last clear). A base read of such a row by
  /// mutation `i` then reads `T_w` instead of `w`. If `T_w ⊆ S_i` for every such read,
  /// every `D_i` stays the same (by induction on `i`), and so does `f`. This is checked
  /// here symbolically.
  fn batch_is_idempotent(
    &self,
    mutations: &[Mutation<'tcx>],
    location: Location,
  ) -> bool {
    if mutations.len() <= 1 {
      return true;
    }
    let controlled_by = self.control_dependencies.dependent_on(location.block);
    let control_keys = controlled_by
      .into_iter()
      .flat_map(|set| set.iter())
      .filter_map(
        |block| match &self.body.basic_blocks[block].terminator().kind {
          TerminatorKind::SwitchInt { discr, .. } => discr.as_place(),
          _ => None,
        },
      )
      .flat_map(|discr| self.influence_keys(discr))
      .copied()
      .collect::<Vec<_>>();

    /// The sources of the value of a row within the batch.
    struct RowSources<'tcx> {
      /// Whether the row still has its value from the pre-state (it is not cleared).
      initial: bool,
      /// The sources of the dependencies written to the row since it was cleared.
      written: HashSet<NormPlace<'tcx>>,
    }
    let mut rows = HashMap::<NormPlace<'tcx>, RowSources<'tcx>>::default();
    let mut base_reads = Vec::with_capacity(mutations.len());
    let mut sources = Vec::with_capacity(mutations.len());
    for mt in mutations {
      let mut base = mt
        .inputs
        .iter()
        .flat_map(|input| self.influence_keys(*input))
        .copied()
        .chain(control_keys.iter().copied())
        .collect::<HashSet<_>>();
      let mut source = base.clone();

      if mt.status() == MutationStatus::Definitely
        && self.place_info.aliases(mt.mutated).len() == 1
      {
        for key in self.children_keys(mt.mutated) {
          rows.insert(*key, RowSources {
            initial: false,
            written: HashSet::default(),
          });
        }
      }

      for key in self.influence_keys(mt.mutated) {
        match rows.get(key) {
          Some(row) => {
            if row.initial {
              base.insert(*key);
              source.insert(*key);
            }
            source.extend(row.written.iter().copied());
          }
          None => {
            base.insert(*key);
            source.insert(*key);
          }
        }
      }

      let written = self
        .written_alias_keys(mt.mutated)
        .iter()
        .copied()
        .chain(self.possibly_shared_rows(mt));
      for key in written {
        rows
          .entry(key)
          .or_insert_with(|| RowSources {
            initial: true,
            written: HashSet::default(),
          })
          .written
          .extend(source.iter().copied());
      }

      base_reads.push(base);
      sources.push(source);
    }

    // `rows` now holds the sources of the value in `f(x)` of every row that `f` clears
    // or writes. A row that is only cleared keeps its value in `x ∨ f(x)`.
    iter::zip(&base_reads, &sources).all(|(base, source)| {
      base.iter().all(|key| match rows.get(key) {
        Some(row) if !row.written.is_empty() => {
          row.written.iter().all(|written| source.contains(written))
        }
        _ => true,
      })
    })
  }

  /// Makes the place queries of [`deps_for`](Self::deps_for) for `place`, which then
  /// only reads the state.
  pub(crate) fn prepare_deps_for(&self, place: Place<'tcx>) {
    for reachable in self.place_info.reachable_values(place, Mutability::Not) {
      self.influence_keys(*reachable);
    }
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
    for reachable in self.place_info.reachable_values(place, Mutability::Not) {
      for key in self.influence_keys(*reachable) {
        deps.union(state.row_set(key));
      }
    }
    deps
  }

  /// The union of the rows of `state` that influence each of `inputs`.
  pub(crate) fn deps_of_inputs<C: IndexedValue + 'static>(
    &self,
    state: &impl RowMatrix<NormPlace<'tcx>, C>,
    inputs: &[Place<'tcx>],
  ) -> IndexSet<C> {
    let mut deps = IndexSet::new(state.col_domain());
    for input in inputs {
      for key in self.influence_keys(*input) {
        deps.union(state.row_set(key));
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
  pub(crate) fn transfer<C, M>(
    &self,
    state: &mut M,
    mutations: &[Mutation<'tcx>],
    location: Location,
    seed: impl Fn(Location, &mut IndexSet<C>),
  ) where
    C: IndexedValue + std::fmt::Debug + 'static,
    M: RowMatrix<NormPlace<'tcx>, C>,
  {
    debug!("  Applying mutations {mutations:?}");
    let counters = &self.counters;
    counters.transfers.set(counters.transfers.get() + 1);
    counters
      .mutations
      .set(counters.mutations.get() + mutations.len());

    // Initialize dependencies to include current location of mutation.
    let mut all_deps = {
      let mut deps = IndexSet::new(state.col_domain());
      seed(location, &mut deps);
      vec![deps; mutations.len()]
    };

    // Add every influence on `input` to `deps`.
    let add_deps = |state: &M, input, target_deps: &mut IndexSet<C>| {
      for key in self.influence_keys(input) {
        target_deps.union(state.row_set(key));
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
        for key in self.children_keys(mt.mutated) {
          state.clear_row(key);
        }
      }

      // Add deps of mutated to include provenance of mutated pointers
      add_deps(state, mt.mutated, deps);

      debug!("    with deps {deps:?}");
      for key in self.written_alias_keys(mt.mutated) {
        state.union_into_row(*key, deps);
      }

      // Pessimistic analysis only: other handles to the same interior-mutable
      // state may point to the object that was just written.
      for row in self.possibly_shared_rows(mt) {
        state.union_into_row(row, deps);
      }
    }
  }

  /// In the pessimistic analysis of
  /// [`compute_flow_with_shared_handles`](super::compute_flow_with_shared_handles),
  /// the states of the other handles that `mutation` may also write (see
  /// [`SharedHandles::possibly_shared`]). Empty in the exact analysis.
  fn possibly_shared_rows(
    &self,
    mutation: &Mutation<'tcx>,
  ) -> SmallVec<[NormPlace<'tcx>; 8]> {
    let Some(handles) = &self.shared_handles else {
      return SmallVec::new();
    };
    self
      .written_aliases(mutation.mutated)
      .iter()
      .flat_map(|alias| handles.possibly_shared(mutation, *alias, &self.place_info))
      .collect()
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
    FlowDomain::new(&self.seeds)
  }

  fn initialize_start_block(&self, _body: &Body<'tcx>, state: &mut Self::Domain) {
    // Every seeded row (a place conflicting with a place reachable from an argument)
    // starts out depending on its argument.
    state.seed();

    // The shadow replays the eager seeding: every row gets its argument explicitly.
    #[cfg(feature = "shadow-eager")]
    {
      for (arg, loc) in self.place_info.all_args() {
        for place in self.place_info.conflicts(arg) {
          state.shadow_insert(self.place_info.normalize(*place), loc);
        }
      }
      state.check_all();
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
      for mutations in self.modular_mutations(location).iter() {
        debug_assert!(definite_writes_disjoint(mutations), "{mutations:?}");
        self.transfer_function(state, mutations, location)
      }
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
