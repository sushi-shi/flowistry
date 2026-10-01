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

pub use self::{
  analysis::{FlowAnalysis, FlowDomain},
  callsite::{FallbackReason, UnsupportedOp},
  dependencies::{
    Direction, compute_dependencies, compute_dependency_spans, compute_focus_spans,
    merge_spans,
  },
  domain::{LazyMatrix, RowGroups, SeedRows},
  session::{AnalysisSession, SummaryStats, SummaryStore},
  summary_wire::PortableSummary,
};
use self::{hidden::HiddenState, shared_handles::SharedHandles};
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
mod domain;
mod effects;
mod hidden;
mod interior;
pub mod mutation;
mod recursive;
mod session;
mod shared_handles;
mod simple_args;
mod summary;
mod summary_wire;

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
///   let Θ: &FlowDomain      = &results.state_at(ℓ);
///   let p: Place            = Place::make(Local::from_usize(1), &[], tcx);
///   let κ: LocationOrArgSet = results.analysis.deps_for(Θ, p);
///   for ℓ2 in κ.iter() {
///     println!("at location {ℓ:?}, place {p:?} depends on location {ℓ2:?}");
///   }
/// }
/// ```
///
/// To access a [`FlowDomain`] for a given location, use the method [`AnalysisResults::state_at`](engine::AnalysisResults::state_at),
/// or [`AnalysisResults::for_each_state`](engine::AnalysisResults::for_each_state) to visit every location: the
/// results only store the state at the entry of each basic block, and recompute the others.
/// See [`FlowDomain`] for more on how to access the location set for a given place.
///
/// **Note:** this analysis uses rustc's [dataflow analysis framework](https://rustc-dev-guide.rust-lang.org/mir/dataflow.html),
/// i.e. [`rustc_mir_dataflow`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/index.html).
/// You will see several types and traits from that crate here, such as
/// [`Analysis`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/trait.Analysis.html) and
/// [`AnalysisDomain`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/trait.AnalysisDomain.html).
/// However, for performance purposes, several constructs were reimplemented within Flowistry, such as [`AnalysisResults`](engine::AnalysisResults)
/// which replaces [`rustc_mir_dataflow::Results`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/struct.Results.html).
pub type FlowResults<'a, 'tcx> =
  engine::AnalysisResults<'a, 'tcx, FlowAnalysis<'a, 'tcx>>;

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
  /// Visits of a basic block by the fixpoint iteration, counting revisits (none if
  /// the location engine ran, see [`by_block`](Self::by_block)).
  pub block_visits: usize,
  /// Whether the block engine computed the results (see [`engine`]).
  pub by_block: bool,
  /// Reachable locations whose effect is not idempotent on its own output; the block
  /// engine only runs if there are none.
  pub unstable_locations: usize,
  /// Joins into a successor's state that changed it.
  pub changed_joins: usize,
  /// Applications of the transfer function (one per visit of a location that mutates
  /// places).
  pub transfers: usize,
  /// Mutations applied by those transfers.
  pub mutations: usize,
  /// Rows seeded from the arguments at the start of the body (see [`SeedRows`]).
  pub seed_rows: usize,
  /// Groups of rows that calls write with one value (see [`RowGroups`]).
  pub row_groups: usize,
  /// Rows in those groups.
  pub grouped_rows: usize,
  /// How often a state stored the value of a group member by member again, so far.
  pub group_expansions: usize,
  /// How often the place queries were made and computed.
  pub place_caches: PlaceCacheStats,
}

impl FlowStats {
  /// The counters as `(name, value)` pairs, in a fixed order.
  pub fn counters(&self) -> Vec<(&'static str, usize)> {
    let mut counters = vec![
      ("locations", self.locations),
      ("location_visits", self.location_visits),
      ("block_visits", self.block_visits),
      ("by_block", self.by_block as usize),
      ("unstable_locations", self.unstable_locations),
      ("changed_joins", self.changed_joins),
      ("transfers", self.transfers),
      ("mutations", self.mutations),
      ("seed_rows", self.seed_rows),
      ("row_groups", self.row_groups),
      ("grouped_rows", self.grouped_rows),
      ("group_expansions", self.group_expansions),
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
  /// Rows stored explicitly (see [`LazyMatrix`]) summed over the states of all locations.
  pub explicit_rows: usize,
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
      block_visits: engine.block_visits,
      by_block: engine.by_block,
      unstable_locations: counters.unstable_locations.get(),
      changed_joins: engine.changed_joins,
      transfers: counters.transfers.get(),
      mutations: counters.mutations.get(),
      seed_rows: self.analysis.seeds.len(),
      row_groups: self.analysis.row_groups.len(),
      grouped_rows: self.analysis.row_groups.members_len(),
      group_expansions: self.analysis.row_groups.expansions(),
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
    self.for_each_state(|_, state| {
      for (_, locations) in state.rows() {
        stats.rows += 1;
        stats.row_entries += locations.count();
      }
      stats.explicit_rows += state.explicit_len();
    });
    stats
  }
}

/// Runs the other engine on a fresh analysis of the body. If `results` come from the
/// block engine, checks that the location engine computes the same state at every
/// location. Otherwise, only logs whether the engines disagree on the body (a
/// `stat engine_diff.unstable_same` or `stat engine_diff.unstable_differs` line).
#[cfg(feature = "engine-diff")]
fn check_engines<'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_with_facts: &BodyWithBorrowckFacts<'tcx>,
  results: &FlowResults<'_, 'tcx>,
) {
  // An unstable body uses the location engine in production. Comparing it only
  // to the block engine can legitimately disagree, hiding a grouping regression.
  // First compare grouped location states to an independent ungrouped execution
  // of that same engine. Drop those states before the existing cross-engine check.
  if !results.engine_stats().by_block && !results.analysis.row_groups.is_empty() {
    let tcx = session.tcx();
    let reference = AnalysisSession::new(tcx, session.mode());
    let body = &body_with_facts.body;
    let def_id = results.analysis.def_id;
    let place_info =
      PlaceInfo::build_with_mode(tcx, def_id, body_with_facts, reference.mode());
    let location_domain = place_info.location_domain().clone();
    let pessimistic = results.analysis.hidden.is_pessimistic();
    let mut analysis =
      FlowAnalysis::with_session(tcx, def_id, body, place_info, reference);
    if pessimistic {
      make_pessimistic(&mut analysis);
    }
    let ungrouped =
      engine::iterate_to_fixpoint_by_location(tcx, body, location_domain, analysis);
    results.for_each_state(|location, state| {
      assert!(
        *state == *ungrouped.state_at(location),
        "row-group-diff: states disagree at {location:?} in {}",
        tcx.def_path_debug_str(def_id)
      );
    });
    assert_eq!(
      *results.analysis.call_reads.borrow(),
      *ungrouped.analysis.call_reads.borrow(),
      "row-group-diff: terminator reads disagree"
    );
  }
  let tcx = session.tcx();
  let def_id = results.analysis.def_id;
  // Reference execution must not add cache hits or fallback counts to the
  // production session being measured and tested.
  let session = AnalysisSession::new(tcx, session.mode());
  let body = &body_with_facts.body;
  let place_info =
    PlaceInfo::build_with_mode(tcx, def_id, body_with_facts, session.mode());
  let location_domain = place_info.location_domain().clone();
  let pessimistic = results.analysis.hidden.is_pessimistic();
  let mut analysis =
    FlowAnalysis::with_session(tcx, def_id, body, place_info, session.clone());
  if pessimistic {
    make_pessimistic(&mut analysis);
  }
  let by_block = results.engine_stats().by_block;
  let other = if by_block {
    engine::iterate_to_fixpoint_by_location(tcx, body, location_domain, analysis)
  } else {
    engine::iterate_to_fixpoint(tcx, body, location_domain, analysis)
  };
  let mut differs = false;
  results.for_each_state(|location, state| {
    let other = other.state_at(location);
    if *state == *other || differs {
      return;
    }
    differs = true;
    let mut rows = state
      .rows()
      .chain(other.rows())
      .map(|(row, _)| *row)
      .filter(|row| state.row_set(row) != other.row_set(row))
      .map(|row| {
        format!(
          "  {row:?}: by block {:?}, by location {:?}",
          state.row_set(&row),
          other.row_set(&row)
        )
      })
      .collect::<Vec<_>>();
    rows.sort();
    rows.dedup();
    let message = format!(
      "engine-diff: the engines disagree at {location:?} ({:?}) in {}:\n{}",
      body.stmt_at(location),
      tcx.def_path_debug_str(def_id),
      rows.join("\n")
    );
    if by_block {
      panic!("{message}");
    }
    log::info!(target: "flowistry::engine_diff", "{message}");
  });
  // The dependencies of what the terminators read (in Recurse mode) are recorded
  // from their pre-states during the fixpoint.
  if by_block
    && *results.analysis.call_reads.borrow() != *other.analysis.call_reads.borrow()
  {
    panic!(
      "engine-diff: the engines disagree on the reads of the terminators in {}",
      tcx.def_path_debug_str(def_id)
    );
  }
  if !by_block {
    let outcome = if differs { "differs" } else { "same" };
    log::info!(target: "flowistry::stats", "stat engine_diff.unstable_{outcome} = 1");
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
/// It also tracks the hidden cells of the pessimistic analysis (see
/// [`hidden`]): the state shared through the operating system, and the memory
/// reached through raw pointers.
///
/// Dependencies present here but not in the result of
/// [`compute_flow_with_session`] are possible, not certain. Returns `None` when
/// neither shared handles nor hidden cells would make the result differ from the
/// exact one.
pub fn compute_flow_with_shared_handles<'a, 'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_id: BodyId,
  body_with_facts: &'a BodyWithBorrowckFacts<'tcx>,
) -> Option<FlowResults<'a, 'tcx>> {
  let place_info = build_place_info(session, body_id, body_with_facts);
  let shared_handles = SharedHandles::build(&place_info);
  let hidden = HiddenState::build(&place_info, true);
  if shared_handles.is_none() && !hidden.has_pessimistic_effects(&place_info, session) {
    return None;
  }
  Some(run_flow(
    session,
    body_with_facts,
    place_info,
    Some((shared_handles, hidden)),
  ))
}

/// Makes `analysis` the pessimistic analysis of
/// [`compute_flow_with_shared_handles`].
#[cfg(feature = "engine-diff")]
fn make_pessimistic(analysis: &mut FlowAnalysis<'_, '_>) {
  analysis.shared_handles = SharedHandles::build(&analysis.place_info);
  analysis.hidden = HiddenState::build(&analysis.place_info, true);
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
  pessimistic: Option<(Option<SharedHandles<'tcx>>, HiddenState<'tcx>)>,
) -> FlowResults<'a, 'tcx> {
  let tcx = session.tcx();
  let mode = session.mode();
  let def_id = place_info.def_id;
  let location_domain = place_info.location_domain().clone();

  let body = &body_with_facts.body;

  let results = {
    log::info!(target: "flowistry::audit", "audit solve {} {}",
      if pessimistic.is_some() { "shared" } else { "focus" }, tcx.def_path_str(def_id));
    block_timer!("Flow");

    let mut analysis =
      FlowAnalysis::with_session(tcx, def_id, body, place_info, session.clone());
    if let Some((shared_handles, hidden)) = pessimistic {
      analysis.shared_handles = shared_handles;
      analysis.hidden = hidden;
    }
    // The block engine stores far fewer states, but it computes the same states as the
    // location engine only if the effect of every location is idempotent on its own
    // output (see `engine`).
    let stats = log::log_enabled!(target: "flowistry::stats", log::Level::Info);
    let unstable = analysis.unstable_locations(if stats { usize::MAX } else { 1 });
    analysis.counters.unstable_locations.set(unstable);
    // Prepare groups even when the instability check stopped at its first hit.
    analysis.build_row_groups();
    if unstable == 0 {
      engine::iterate_to_fixpoint(tcx, body, location_domain, analysis)
    } else {
      engine::iterate_to_fixpoint_by_location(tcx, body, location_domain, analysis)
    }
    // analysis.into_engine(tcx, body).iterate_to_fixpoint()
  };

  #[cfg(feature = "engine-diff")]
  check_engines(session, body_with_facts, &results);

  if log::log_enabled!(target: "flowistry::stats", log::Level::Info) {
    for (name, value) in results.stats().counters() {
      log::info!(target: "flowistry::stats", "stat {name} = {value}");
    }
  }

  if log::log_enabled!(log::Level::Info) {
    let FlowSizeStats {
      locations: nloc,
      rows: np,
      explicit_rows: ne,
      row_entries: nl,
    } = results.size_stats();
    let pavg = np as f64 / (nloc as f64);
    let lavg = nl as f64 / (nloc as f64);
    log::info!(
      "Over {nloc} locations, total number of place entries: {np} (avg {pavg:.0}/loc, {ne} stored), total size of location sets: {nl} (avg {lavg:.0}/loc)",
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

  fn engine_stats_of(input: &str) -> FlowStats {
    let stats = std::sync::Mutex::new(None);
    test_utils::compile_body(input, |tcx, body_id, body_with_facts| {
      let results = compute_flow(tcx, body_id, body_with_facts);
      // Reading the states replays blocks: `state_at` and `for_each_state` agree.
      results.for_each_state(|location, state| {
        assert!(*state == *results.state_at(location), "{location:?}");
      });
      *stats.lock().unwrap() = Some(results.stats());
    });
    stats.into_inner().unwrap().unwrap()
  }

  /// A body whose effects are all idempotent on their own output runs on the block
  /// engine.
  #[test]
  fn test_block_engine_when_exact() {
    let stats = engine_stats_of(
      r#"
fn f(x: &mut (i32, i32), v: Vec<i32>, n: usize) -> i32 {
  let mut y = 0;
  for i in 0 .. n {
    x.0 = y + v[i];
    y = foo(x, y);
  }
  y
}
fn foo(x: &mut (i32, i32), y: i32) -> i32 { x.1 + y }
"#,
    );
    assert!(stats.by_block, "{stats:?}");
    assert_eq!(stats.unstable_locations, 0);
    assert!(stats.block_visits > 0);
  }

  /// A call through raw pointers writes every place behind a raw pointer, including
  /// the pointees its own inputs read: applying it to its own output adds dependencies,
  /// so the location engine runs (see the fixture `revisited_raw_pointer_call`).
  #[test]
  fn test_location_engine_when_not_idempotent() {
    let stats = engine_stats_of(
      r#"
fn f(p: *const u8, r: *const u8, n: usize) -> usize {
  let mut i = 0;
  while i < n { i += 1; }
  let a = p;
  g(a, a)
}
fn g(a: *const u8, b: *const u8) -> usize { 0 }
"#,
    );
    assert!(!stats.by_block, "{stats:?}");
    assert!(stats.unstable_locations > 0);
    assert_eq!(stats.block_visits, 0);
  }
}

#[cfg(test)]
mod row_group_test {
  use super::*;
  use crate::test_utils;

  /// A callee returning a large enum, called from three sites. Its summary writes
  /// every leaf of the returned value with the dependencies of the whole value, so each
  /// call writes the leaves of its destination as one row group (see
  /// `FlowAnalysis::build_row_groups`).
  const BIG_ENUM: &str = r#"
enum Big {
  A(u8, u16, u32, u64),
  B(i8, i16, i32, i64),
  C(bool, char, (u8, u8), (u16, u16)),
  D { x: u32, y: u32, z: u32 },
}

fn make(n: u32) -> Result<u32, Big> {
  if n > 3 { Err(Big::D { x: n, y: n, z: n }) } else { Ok(n) }
}

fn f(n: u32, m: u32) -> u32 {
  let mut total = 0;
  for i in 0 .. n {
    let a = make(i);
    let b = make(m);
    if let Err(Big::D { x, .. }) = a { total += x; }
    if let Ok(k) = b { total += k; }
  }
  let c = make(total);
  if let Err(Big::A(p, ..)) = c { total += p as u32; }
  total
}
"#;

  /// The states computed with row groups equal those computed row by row, with far
  /// fewer stored rows, and the three calls write three groups.
  #[test]
  fn test_row_groups_are_exact() {
    test_utils::compile_crate(BIG_ENUM, &[], |tcx| {
      let (def_id, body_with_facts) = test_utils::body_named(tcx, "f");
      let body_id = tcx.hir_body_owned_by(def_id).id();
      let mode = EvalMode {
        context_mode: ContextMode::Recurse,
        ..EvalMode::default()
      };
      let session = AnalysisSession::new(tcx, mode);
      let grouped = compute_flow_with_session(&session, body_id, body_with_facts);
      let stats = grouped.stats();
      assert_eq!(stats.row_groups, 3, "{stats:?}");
      assert!(stats.grouped_rows >= 3 * 16, "{stats:?}");

      // The same analysis, without row groups.
      let place_info = build_place_info(&session, body_id, body_with_facts);
      let location_domain = place_info.location_domain().clone();
      let body = &body_with_facts.body;
      let analysis =
        FlowAnalysis::with_session(tcx, place_info.def_id, body, place_info, session);
      analysis.unstable_locations(1);
      let ungrouped =
        engine::iterate_to_fixpoint_by_location(tcx, body, location_domain, analysis);
      assert_eq!(ungrouped.stats().row_groups, 0);

      grouped.for_each_state(|location, state| {
        assert!(*state == *ungrouped.state_at(location), "{location:?}");
      });
      let (grouped_size, ungrouped_size) = (grouped.size_stats(), ungrouped.size_stats());
      assert_eq!(grouped_size.rows, ungrouped_size.rows);
      assert_eq!(grouped_size.row_entries, ungrouped_size.row_entries);
      // The members are written as groups at every visit of the calls, and never
      // on their own.
      assert_eq!(grouped.stats().group_expansions, 0);
      assert!(
        grouped_size.explicit_rows * 2 < ungrouped_size.explicit_rows,
        "{grouped_size:?} {ungrouped_size:?}"
      );
    });
  }
}
