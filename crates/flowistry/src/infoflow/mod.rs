//! The core information flow analysis.
//!
//! The main function is [`compute_flow`]. See [`FlowResults`] and [`FlowDomain`] for an explanation
//! of what it returns.

use std::rc::Rc;

use log::debug;
use rustc_borrowck::consumers::BodyWithBorrowckFacts;
use rustc_hir::BodyId;
use rustc_middle::ty::TyCtxt;
use rustc_utils::{BodyExt, block_timer};

use self::shared_handles::SharedHandles;
pub use self::{
  analysis::{FlowAnalysis, FlowDomain},
  callsite::{FallbackReason, UnsupportedOp},
  dependencies::{
    Direction, compute_dependencies, compute_dependency_spans, compute_focus_spans,
  },
  session::{AnalysisSession, SummaryStats},
};
use crate::{
  extensions::{ContextMode, EvalMode},
  mir::{
    bitset::IndexSetExt,
    engine,
    placeinfo::{PlaceCacheStats, PlaceInfo},
    utils::MAX_ARG_POINTER_DEPTH,
  },
};

mod analysis;
mod callsite;
mod dependencies;
mod effects;
mod interior;
pub mod mutation;
mod recursive;
mod session;
mod shared_handles;
mod simple_args;
mod summary;

/// The output of the information flow analysis.
///
/// Using the metavariables in [the paper](https://arxiv.org/abs/2111.13662): for each
/// [`LocationOrArg`](rustc_utils::mir::location_or_arg::LocationOrArg) $\ell$ in a [`Body`](rustc_middle::mir::Body) $f$,
/// this type contains a [`FlowDomain`] $\Theta_\ell$ that maps from a [`Place`](rustc_middle::mir::Place) $p$
/// (stored in normal form as a [`NormPlace`](crate::mir::placeinfo::NormPlace))
/// to a [`LocationOrArgSet`](rustc_utils::mir::location_or_arg::index::LocationOrArgSet) $\kappa$. The domain of $\Theta_\ell$
/// is all places that have been defined up to $\ell$. For each place, $\Theta_\ell(p)$ contains the set of locations
/// (or arguments) that could influence the value of that place, i.e. the place's dependencies.
///
/// For example, to get the dependencies of the first argument at the first instruction, that would be:
/// ```
/// # #![feature(rustc_private)]
/// # extern crate rustc_middle;
/// # use rustc_middle::{ty::TyCtxt, mir::{Place, Location, Local}};
/// # use flowistry::{infoflow::{FlowDomain, FlowResults}};
/// # use rustc_utils::{mir::location_or_arg::index::LocationOrArgSet, PlaceExt};
/// fn example<'tcx>(tcx: TyCtxt<'tcx>, results: &FlowResults<'_, 'tcx>) {
///   let ℓ: Location         = Location::START;
///   let Θ: &FlowDomain      = results.state_at(ℓ);
///   let p: Place            = Place::make(Local::from_usize(1), &[], tcx);
///   let κ: LocationOrArgSet = results.analysis.deps_for(Θ, p);
///   for ℓ2 in κ.iter() {
///     println!("at location {ℓ:?}, place {p:?} depends on location {ℓ2:?}");
///   }
/// }
/// ```
///
/// To access a [`FlowDomain`] for a given location, use the method [`AnalysisResults::state_at`](engine::AnalysisResults::state_at).
/// See [`FlowDomain`] for more on how to access the location set for a given place.
///
/// **Note:** this analysis uses rustc's [dataflow analysis framework](https://rustc-dev-guide.rust-lang.org/mir/dataflow.html),
/// i.e. [`rustc_mir_dataflow`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/index.html).
/// You will see several types and traits from that crate here, such as
/// [`Analysis`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/trait.Analysis.html) and
/// [`AnalysisDomain`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/trait.AnalysisDomain.html).
/// However, for performance purposes, several constructs were reimplemented within Flowistry, such as [`AnalysisResults`](engine::AnalysisResults)
/// which replaces [`rustc_mir_dataflow::Results`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/struct.Results.html).
pub type FlowResults<'a, 'tcx> = engine::AnalysisResults<'tcx, FlowAnalysis<'a, 'tcx>>;

/// Counters of one run of the analysis on one body, see [`FlowResults::stats`].
///
/// They are cheap to collect. With the `flowistry::stats` log target at `info` level,
/// [`compute_flow`] logs them as `stat <name> = <value>` lines (see [`FlowStats::counters`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowStats {
  /// Locations of the body.
  pub locations: usize,
  /// Statement and terminator effects applied by the fixpoint iteration, counting
  /// every revisit of a location.
  pub location_visits: usize,
  /// Joins into a successor's state that changed it.
  pub changed_joins: usize,
  /// Applications of the transfer function (one per visit of a location that mutates
  /// places).
  pub transfers: usize,
  /// Mutations applied by those transfers.
  pub mutations: usize,
  /// How often the place queries were made and computed.
  pub place_caches: PlaceCacheStats,
}

impl FlowStats {
  /// The counters as `(name, value)` pairs, in a fixed order.
  pub fn counters(&self) -> Vec<(&'static str, usize)> {
    let mut counters = vec![
      ("locations", self.locations),
      ("location_visits", self.location_visits),
      ("changed_joins", self.changed_joins),
      ("transfers", self.transfers),
      ("mutations", self.mutations),
    ];
    counters.extend(self.place_caches.counters());
    counters
  }
}

/// The size of the results of the analysis on one body, see [`FlowResults::size_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowSizeStats {
  /// Locations of the body.
  pub locations: usize,
  /// Rows (places with a non-empty dependency set) summed over the states of all locations.
  pub rows: usize,
  /// Sizes of the dependency sets of those rows, summed.
  pub row_entries: usize,
}

impl<'tcx> FlowResults<'_, 'tcx> {
  /// Counters of the analysis that computed these results. Cheap.
  ///
  /// The place-query counters include the queries made after the fixpoint so far, e.g.
  /// by [`compute_dependencies`].
  pub fn stats(&self) -> FlowStats {
    let engine = self.engine_stats();
    let counters = &self.analysis.counters;
    FlowStats {
      locations: self.analysis.body.all_locations().count(),
      location_visits: engine.location_visits,
      changed_joins: engine.changed_joins,
      transfers: counters.transfers.get(),
      mutations: counters.mutations.get(),
      place_caches: self.analysis.place_info.cache_stats(),
    }
  }

  /// The size of these results. Iterates over every row of every state, so it is
  /// linear in the size of the results.
  pub fn size_stats(&self) -> FlowSizeStats {
    let body = self.analysis.body;
    let mut stats = FlowSizeStats {
      locations: body.all_locations().count(),
      ..FlowSizeStats::default()
    };
    for loc in body.all_locations() {
      for (_, locations) in self.state_at(loc).rows() {
        stats.rows += 1;
        stats.row_entries += locations.count();
      }
    }
    stats
  }
}

/// Computes information flow for a MIR body.
///
/// See [example.rs](https://github.com/willcrichton/flowistry/tree/master/crates/flowistry/examples/example.rs)
/// for a complete example of how to call this function.
///
/// To get a `BodyWithBorrowckFacts`, you can use the
/// [`get_body_with_borrowck_facts`](rustc_utils::mir::borrowck_facts::get_body_with_borrowck_facts)
/// function.
///
/// See [`FlowResults`] for an explanation of how to use the return value.
///
/// The analysis runs with the ambient [`EvalMode`] (see [`EvalMode::from_ambient`]),
/// which is read once at entry. Use [`compute_flow_with_mode`] to pass it explicitly.
pub fn compute_flow<'a, 'tcx>(
  tcx: TyCtxt<'tcx>,
  body_id: BodyId,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
) -> FlowResults<'a, 'tcx> {
  compute_flow_with_mode(tcx, body_id, body_with_facts, EvalMode::from_ambient())
}

/// Computes information flow for a MIR body with an explicit [`EvalMode`].
///
/// See [`compute_flow`] for details. The mode is also used for every callee analyzed
/// in [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse). The callee
/// summaries are computed for this body only: use [`compute_flow_with_session`] to
/// share them between bodies.
pub fn compute_flow_with_mode<'a, 'tcx>(
  tcx: TyCtxt<'tcx>,
  body_id: BodyId,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
  mode: EvalMode,
) -> FlowResults<'a, 'tcx> {
  compute_flow_with_session(&AnalysisSession::new(tcx, mode), body_id, body_with_facts)
}

/// Computes information flow for a MIR body in the mode of `session`, reusing (and
/// adding to) the callee summaries of `session`.
///
/// See [`compute_flow`] for details. The session must belong to the compiler session
/// of `body_with_facts`.
pub fn compute_flow_with_session<'a, 'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_id: BodyId,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
) -> FlowResults<'a, 'tcx> {
  let place_info = build_place_info(session, body_id, body_with_facts);
  run_flow(session, body_with_facts, place_info, None)
}

/// Computes information flow for a MIR body like [`compute_flow_with_session`], but
/// assuming that separately held shared handles to state of the same
/// interior-mutable type (e.g. two `Rc<RefCell<T>>`, or two `&Cell<T>`) may point to
/// the same object: a write to the state of one handle possibly writes the state of
/// the others.
///
/// Dependencies present here but not in the result of
/// [`compute_flow_with_session`] are possible, not certain. Returns `None` when no
/// two handles of the body share a state type, as the result would then equal the
/// exact one.
pub fn compute_flow_with_shared_handles<'a, 'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_id: BodyId,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
) -> Option<FlowResults<'a, 'tcx>> {
  let place_info = build_place_info(session, body_id, body_with_facts);
  let shared_handles = SharedHandles::build(&place_info)?;
  Some(run_flow(
    session,
    body_with_facts,
    place_info,
    Some(shared_handles),
  ))
}

fn build_place_info<'a, 'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_id: BodyId,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
) -> PlaceInfo<'a, 'tcx> {
  let tcx = session.tcx();
  debug!("{}", body_with_facts.body.to_string(tcx).unwrap());
  let def_id = tcx.hir_body_owner_def_id(body_id).to_def_id();
  let place_info =
    PlaceInfo::build_with_mode(tcx, def_id, body_with_facts, session.mode());
  if log::log_enabled!(log::Level::Debug) && place_info.arg_pointers_truncated() {
    debug!(
      "Arguments hold pointers nested deeper than {MAX_ARG_POINTER_DEPTH} projections; the loans behind them are ignored"
    );
  }
  place_info
}

fn run_flow<'a, 'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
  place_info: PlaceInfo<'a, 'tcx>,
  shared_handles: Option<SharedHandles<'tcx>>,
) -> FlowResults<'a, 'tcx> {
  let tcx = session.tcx();
  let mode = session.mode();
  let def_id = place_info.def_id;
  let location_domain = place_info.location_domain().clone();

  let body = &body_with_facts.body;

  let results = {
    block_timer!("Flow");

    let mut analysis =
      FlowAnalysis::with_session(tcx, def_id, body, place_info, session.clone());
    analysis.shared_handles = shared_handles;
    engine::iterate_to_fixpoint(tcx, body, location_domain, analysis)
    // analysis.into_engine(tcx, body).iterate_to_fixpoint()
  };

  if log::log_enabled!(target: "flowistry::stats", log::Level::Info) {
    for (name, value) in results.stats().counters() {
      log::info!(target: "flowistry::stats", "stat {name} = {value}");
    }
  }

  if log::log_enabled!(log::Level::Info) {
    let FlowSizeStats {
      locations: nloc,
      rows: np,
      row_entries: nl,
    } = results.size_stats();
    let pavg = np as f64 / (nloc as f64);
    let lavg = nl as f64 / (nloc as f64);
    log::info!(
      "Over {nloc} locations, total number of place entries: {np} (avg {pavg:.0}/loc), total size of location sets: {nl} (avg {lavg:.0}/loc)",
    );
    if mode.context_mode == ContextMode::Recurse {
      log::info!("Callee summaries so far: {:?}", session.stats());
    }
  }

  if std::env::var("DUMP_MIR").is_ok() {
    todo!()
    // utils::dump_results(body, &results, def_id, tcx).unwrap();
  }

  results
}

#[cfg(test)]
mod test {
  use rustc_utils::BodyExt;

  use super::*;
  use crate::test_utils;

  #[test]
  fn test_flow_stats() {
    let input = r#"
fn f(x: i32, v: &mut Vec<i32>) -> i32 {
  let mut y = x;
  for i in 0 .. 3 {
    y += i;
    v.push(y);
  }
  y
}
"#;
    test_utils::compile_body(input, |tcx, body_id, body_with_facts| {
      let results = compute_flow(tcx, body_id, body_with_facts);
      let stats = results.stats();
      let locations = body_with_facts.body.all_locations().count();
      assert_eq!(stats.locations, locations);
      // Every location is visited at least once, and the loop more than once.
      assert!(stats.location_visits > locations, "{stats:?}");
      assert!(stats.changed_joins > 0, "{stats:?}");
      assert!(stats.transfers > 0 && stats.transfers <= stats.location_visits);
      assert!(stats.mutations >= stats.transfers);
      let normalize = stats.place_caches.normalize;
      assert!(normalize.misses > 0 && normalize.misses <= normalize.lookups);

      let size = results.size_stats();
      assert_eq!(size.locations, locations);
      assert!(size.rows > 0 && size.row_entries >= size.rows, "{size:?}");
    });
  }
}
