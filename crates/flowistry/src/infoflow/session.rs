//! Callee summaries shared by the analyses of one compiler session.
//!
//! Rustc identities and borrow-checker facts are meaningful only in this compiler
//! session. An optional store exchanges audited portable summary payloads.

use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

use rustc_borrowck::consumers::BodyWithBorrowckFacts;
use rustc_data_structures::{
  fx::{FxHashMap, FxHashSet},
  graph::{scc::Sccs, vec_graph::VecGraph},
};
use rustc_hir::def_id::LocalDefId;
use rustc_middle::{mir::TerminatorKind, ty::TyCtxt};

use super::{
  callsite::{CalleeAbi, FallbackReason},
  recursive::resolve_callee,
  summary::{self, CalleeSummary},
  summary_wire::PortableSummary,
};
use crate::{
  extensions::EvalMode,
  mir::borrowck::body_with_borrowck_facts as get_body_with_borrowck_facts,
};

/// Counters of an [`AnalysisSession`].
#[derive(Clone, Debug, Default)]
pub struct SummaryStats {
  /// Callee summaries computed, including those of bodies that cannot be summarized.
  pub computations: usize,
  /// Summaries (or reasons why there is none) served from the cache.
  pub cache_hits: usize,
  /// Summaries restored from a compiler-validated persistent store.
  pub persistent_hits: usize,
  /// Persistent lookups absent or rejected by structural validation.
  pub persistent_misses: usize,
  /// How many call sites were analyzed with the modular approximation, by reason.
  pub fallbacks: BTreeMap<FallbackReason, usize>,
  /// Total time spent computing summaries (nested computations counted once).
  pub construction_time: Duration,
}

/// Compiler-aware storage supplied by the IDE layer. Implementations must key
/// payloads by the current backend/compiler, mode, configuration, declaration
/// context and resolved semantic dependency fingerprints. A body name alone is
/// never a sufficient key. The session rebuilds call resolution and SCCs before
/// consulting this interface; no saved compiler identity is trusted.
pub trait SummaryStore<'tcx> {
  /// Return a payload only after validating its key and integrity.
  fn load(
    &self,
    session: &AnalysisSession<'tcx>,
    callee: LocalDefId,
  ) -> Option<PortableSummary>;
  /// Store a completed summary (or fallback reason) without retaining rustc data.
  fn save(
    &self,
    session: &AnalysisSession<'tcx>,
    callee: LocalDefId,
    payload: &PortableSummary,
  );
}

/// State shared by the flow analyses of the bodies of one compiler session with one
/// [`EvalMode`]: in [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse),
/// the summaries of the callees, which are computed at most once.
///
/// Use [`compute_flow_with_session`](super::compute_flow_with_session) to analyze
/// several bodies with one session.
///
/// Calls between functions of one strongly connected component of the call graph
/// (i.e. recursive calls) are analyzed with the modular approximation, so the
/// summaries and the results do not depend on the order in which bodies are
/// analyzed.
pub struct AnalysisSession<'tcx> {
  tcx: TyCtxt<'tcx>,
  mode: EvalMode,
  summaries: RefCell<FxHashMap<LocalDefId, Result<Rc<CalleeSummary>, FallbackReason>>>,
  /// The local callees of each explored body.
  graph: RefCell<FxHashMap<LocalDefId, Vec<LocalDefId>>>,
  /// The strongly connected component of each explored body.
  components: RefCell<FxHashMap<LocalDefId, usize>>,
  /// The summaries being computed.
  active: RefCell<FxHashSet<LocalDefId>>,
  stats: RefCell<SummaryStats>,
  store: Option<Rc<dyn SummaryStore<'tcx> + 'tcx>>,
}

impl<'tcx> AnalysisSession<'tcx> {
  /// Creates a session analyzing with `mode`.
  pub fn new(tcx: TyCtxt<'tcx>, mode: EvalMode) -> Rc<Self> {
    Self::create(tcx, mode, None)
  }

  /// Creates a session using a compiler-validated portable summary store.
  pub fn with_summary_store(
    tcx: TyCtxt<'tcx>,
    mode: EvalMode,
    store: Rc<dyn SummaryStore<'tcx> + 'tcx>,
  ) -> Rc<Self> {
    Self::create(tcx, mode, Some(store))
  }

  fn create(
    tcx: TyCtxt<'tcx>,
    mode: EvalMode,
    store: Option<Rc<dyn SummaryStore<'tcx> + 'tcx>>,
  ) -> Rc<Self> {
    Rc::new(AnalysisSession {
      tcx,
      mode,
      summaries: RefCell::default(),
      graph: RefCell::default(),
      components: RefCell::default(),
      active: RefCell::default(),
      stats: RefCell::default(),
      store,
    })
  }

  /// The mode of every analysis of this session.
  pub fn mode(&self) -> EvalMode {
    self.mode
  }

  /// A snapshot of the counters of this session.
  pub fn stats(&self) -> SummaryStats {
    self.stats.borrow().clone()
  }

  /// The type context of this session.
  pub fn tcx(&self) -> TyCtxt<'tcx> {
    self.tcx
  }

  /// Local bodies contributing to a root's recursive analysis, including cycles.
  /// These identities are valid only within this compiler session.
  pub fn dependencies(&self, root: LocalDefId) -> Vec<LocalDefId> {
    self.explore(root);
    let graph = self.graph.borrow();
    let mut seen = FxHashSet::default();
    let mut pending = vec![root];
    while let Some(def) = pending.pop() {
      if seen.insert(def) {
        pending.extend(graph[&def].iter().copied());
      }
    }
    seen.into_iter().collect()
  }

  /// Current compiler-resolved direct local callees. Consumers must translate
  /// these session-local IDs into stable identities before persisting edges.
  pub fn direct_dependencies(&self, root: LocalDefId) -> Vec<LocalDefId> {
    self.explore(root);
    self.graph.borrow()[&root].clone()
  }

  pub(crate) fn record_fallback(&self, reason: FallbackReason) {
    *self.stats.borrow_mut().fallbacks.entry(reason).or_default() += 1;
  }

  pub(crate) fn body(&self, def_id: LocalDefId) -> &'tcx BodyWithBorrowckFacts<'tcx> {
    get_body_with_borrowck_facts(self.tcx, def_id)
  }

  /// Whether `caller` and `callee` are in the same strongly connected component of
  /// the call graph, i.e. whether the call is recursive.
  pub(crate) fn same_component(&self, caller: LocalDefId, callee: LocalDefId) -> bool {
    self.explore(caller);
    let components = self.components.borrow();
    components.get(&caller) == components.get(&callee)
  }

  /// Adds the bodies reachable from `root` to the call graph, and recomputes its
  /// strongly connected components.
  ///
  /// The components of bodies explored earlier do not change: their callees were
  /// all explored with them, so no new body can reach back to them.
  fn explore(&self, root: LocalDefId) {
    if self.graph.borrow().contains_key(&root) {
      return;
    }
    let mut pending = vec![root];
    while let Some(def_id) = pending.pop() {
      if self.graph.borrow().contains_key(&def_id) {
        continue;
      }
      let body = &self.body(def_id).body;
      let mut callees = body
        .basic_blocks
        .iter()
        .filter_map(|data| match &data.terminator().kind {
          TerminatorKind::Call { func, .. } => {
            resolve_callee(self.tcx, def_id, func).ok()
          }
          _ => None,
        })
        .collect::<Vec<_>>();
      callees.sort_by_key(|def_id| def_id.local_def_index);
      callees.dedup();
      pending.extend(callees.iter().copied());
      self.graph.borrow_mut().insert(def_id, callees);
    }

    let graph = self.graph.borrow();
    let mut nodes = graph.keys().copied().collect::<Vec<_>>();
    nodes.sort_by_key(|def_id| def_id.local_def_index);
    let indices = nodes
      .iter()
      .enumerate()
      .map(|(i, def_id)| (*def_id, i))
      .collect::<FxHashMap<_, _>>();
    let edges = nodes
      .iter()
      .flat_map(|from| {
        graph[from]
          .iter()
          .map(|to| (indices[from], indices[to]))
          .collect::<Vec<_>>()
      })
      .collect::<Vec<_>>();
    let graph = VecGraph::<usize, false>::new(nodes.len(), edges);
    let sccs = Sccs::<usize, usize>::new(&graph);
    *self.components.borrow_mut() = nodes
      .into_iter()
      .enumerate()
      .map(|(i, def_id)| (def_id, sccs.scc(i)))
      .collect();
  }

  /// The summary of `callee` (cached), or why there is none.
  pub(crate) fn summary(
    self: &Rc<Self>,
    callee: LocalDefId,
  ) -> Result<Rc<CalleeSummary>, FallbackReason> {
    if let Some(summary) = self.summaries.borrow().get(&callee) {
      self.stats.borrow_mut().cache_hits += 1;
      return summary.clone();
    }
    // Always rebuild the current resolved graph, including recursive components.
    // A disk hit must not bypass the recursion policy or install saved DefIds.
    let abi = self.store.as_ref().map(|_| {
      self.explore(callee);
      CalleeAbi::of_body(self.tcx, callee.to_def_id(), &self.body(callee).body)
    });
    if let (Some(store), Some(abi)) = (&self.store, abi) {
      if let Some(summary) = store.load(self, callee).and_then(|wire| wire.restore(abi)) {
        self.stats.borrow_mut().persistent_hits += 1;
        self.summaries.borrow_mut().insert(callee, summary.clone());
        log::info!(target: "flowistry::audit", "audit summary-hit {}", self.tcx.def_path_str(callee));
        return summary;
      }
      self.stats.borrow_mut().persistent_misses += 1;
    }
    // Recursive calls are not summarized (see `same_component`), so a summary never
    // depends on itself.
    assert!(
      self.active.borrow_mut().insert(callee),
      "cyclic summary of {callee:?}"
    );
    let start = std::time::Instant::now();
    let nested_before = self.stats.borrow().construction_time;
    let summary = summary::compute(self, callee).map(Rc::new);
    let elapsed = start.elapsed();
    self.active.borrow_mut().remove(&callee);
    log::debug!(
      "Summary of {}: {:?} in {elapsed:?}",
      self.tcx.def_path_str(callee),
      summary.as_ref().map(|_| ())
    );
    self.summaries.borrow_mut().insert(callee, summary.clone());
    {
      let mut stats = self.stats.borrow_mut();
      stats.computations += 1;
      // Nested computations already added their time.
      stats.construction_time = nested_before + elapsed;
    }
    if let (Some(store), Some(abi)) = (&self.store, abi) {
      if let Some(wire) = PortableSummary::capture(abi, &summary) {
        store.save(self, callee, &wire);
      }
    }
    summary
  }
}

impl Drop for AnalysisSession<'_> {
  fn drop(&mut self) {
    let stats = self.stats.get_mut();
    if stats.computations > 0 || stats.persistent_hits > 0 {
      log::info!("Callee summaries: {stats:?}");
    }
  }
}
