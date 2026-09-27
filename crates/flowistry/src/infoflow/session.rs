//! Compiler-scoped summary ownership. Nothing here survives its `TyCtxt`.

use std::{cell::RefCell, rc::Rc, time::Duration};

use rustc_borrowck::consumers::BodyWithBorrowckFacts;
use rustc_data_structures::{
  fx::{FxHashMap, FxHashSet},
  graph::{scc::Sccs, vec_graph::VecGraph},
};
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::{Operand, TerminatorKind},
  ty::{self, Instance, TyCtxt},
};
use rustc_utils::mir::borrowck_facts::get_body_with_borrowck_facts;

use super::summary::{self, CalleeSummary};
use crate::extensions::{EVAL_MODE, EvalMode, REACHED_LIBRARY};

/// Counters for one compiler session; logging never changes the JSON protocol.
#[derive(Clone, Debug, Default)]
pub struct SummaryStats {
  /// Completed summary computations, including unsupported-body checks.
  pub computations: usize,
  /// Requests served from the completed summary cache.
  pub cache_hits: usize,
  /// Conservative call effects, grouped by reason.
  pub fallbacks: FxHashMap<&'static str, usize>,
  /// Total summary construction time (nested builds counted once).
  pub construction_time: Duration,
}

/// Shared analysis state for a single compiler invocation and evaluation mode.
///
/// Use [`super::compute_flow_with_session`] to reuse summaries across roots. A
/// session is deliberately neither global nor serializable: rustc identities
/// and borrow-checker facts are only valid in the originating compiler context.
pub struct AnalysisSession<'tcx> {
  tcx: TyCtxt<'tcx>,
  mode: EvalMode,
  summaries: RefCell<FxHashMap<Instance<'tcx>, Option<Rc<CalleeSummary<'tcx>>>>>,
  graph: RefCell<FxHashMap<DefId, Vec<DefId>>>,
  components: RefCell<FxHashMap<DefId, usize>>,
  stats: RefCell<SummaryStats>,
  active: RefCell<FxHashSet<DefId>>,
}

impl<'tcx> AnalysisSession<'tcx> {
  /// Creates a session with the currently selected evaluation mode.
  pub fn new(tcx: TyCtxt<'tcx>) -> Rc<Self> {
    Rc::new(Self {
      tcx,
      mode: EVAL_MODE.copied().unwrap_or_default(),
      summaries: RefCell::default(),
      graph: RefCell::default(),
      components: RefCell::default(),
      stats: RefCell::default(),
      active: RefCell::default(),
    })
  }

  /// The mode is fixed for the lifetime of this session.
  pub fn mode(&self) -> EvalMode {
    self.mode
  }

  /// Returns a snapshot of cache and fallback instrumentation.
  pub fn stats(&self) -> SummaryStats {
    self.stats.borrow().clone()
  }

  /// Local bodies whose implementation can contribute to a root's recursive
  /// analysis, including cycles. These are compiler-local identities only.
  pub fn dependencies(&self, root: DefId) -> Vec<DefId> {
    self.prepare(root);
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

  pub(crate) fn fallback(&self, reason: &'static str) {
    *self.stats.borrow_mut().fallbacks.entry(reason).or_default() += 1;
    log::debug!("Summary fallback: {reason}");
  }

  pub(crate) fn body(&self, def: DefId) -> &'tcx BodyWithBorrowckFacts<'tcx> {
    get_body_with_borrowck_facts(self.tcx, def.expect_local())
  }

  pub(crate) fn resolve(
    &self,
    caller: DefId,
    func: &Operand<'tcx>,
  ) -> Option<Instance<'tcx>> {
    let result = self.resolve_inner(caller, func);
    if let Err(reason) = result {
      self.fallback(reason);
      if reason == "external implementation" {
        REACHED_LIBRARY.get(|reached| {
          if let Some(reached) = reached {
            *reached.borrow_mut() = true;
          }
        });
      }
    }
    result.ok()
  }

  fn resolve_inner(
    &self,
    caller: DefId,
    func: &Operand<'tcx>,
  ) -> Result<Instance<'tcx>, &'static str> {
    let (def, args) = func.const_fn_def().ok_or("indirect call")?;
    let env = self.tcx.typing_env_normalized_for_post_analysis(caller);
    let instance = Instance::try_resolve(self.tcx, env, def, args)
      .ok()
      .flatten()
      .ok_or("unresolved instance")?;
    if !matches!(instance.def, ty::InstanceKind::Item(_)) {
      return Err("compiler shim");
    }
    if instance.args.types().any(|ty| ty.walk().any(|arg| {
      matches!(arg.as_type().map(|ty| ty.kind()), Some(ty::TyKind::Closure(_, args))
        if matches!(args.as_closure().kind(), ty::ClosureKind::FnMut | ty::ClosureKind::FnOnce))
    })) { return Err("mutable or consuming callback captures"); }
    let def = instance.def_id();
    if !def.is_local() {
      return Err("external implementation");
    }
    if self
      .tcx
      .hir_get_if_local(def)
      .and_then(|node| node.body_id())
      .is_none()
    {
      return Err("unavailable body");
    }
    Ok(self.tcx.erase_and_anonymize_regions(instance))
  }

  // Bodies are analyzed parametrically in their own typing environment. Unknown
  // generic dispatch stays opaque even if an outer call supplies concrete types.
  // Consequently this definition graph is stable across instantiations, and an
  // expanding generic recursion cannot create an unbounded instance graph.
  pub(crate) fn prepare(&self, root: DefId) {
    if self.graph.borrow().contains_key(&root) {
      return;
    }
    let mut pending = vec![root];
    while let Some(def) = pending.pop() {
      if self.graph.borrow().contains_key(&def) {
        continue;
      }
      let body = &self.body(def).body;
      let callees = body
        .basic_blocks
        .iter()
        .filter_map(|block| {
          let TerminatorKind::Call { func, .. } = &block.terminator().kind else {
            return None;
          };
          self
            .resolve_inner(def, func)
            .ok()
            .map(|instance| instance.def_id())
        })
        .collect::<Vec<_>>();
      pending.extend(callees.iter().copied());
      self.graph.borrow_mut().insert(def, callees);
    }
    let graph = self.graph.borrow();
    let nodes = graph.keys().copied().collect::<Vec<_>>();
    let indexes = nodes
      .iter()
      .enumerate()
      .map(|(i, def)| (*def, i))
      .collect::<FxHashMap<_, _>>();
    let edges = graph
      .iter()
      .flat_map(|(from, tos)| tos.iter().map(|to| (indexes[from], indexes[to])))
      .collect();
    let graph = VecGraph::<usize, false>::new(nodes.len(), edges);
    let sccs = Sccs::<usize, usize>::new(&graph);
    *self.components.borrow_mut() = nodes
      .into_iter()
      .enumerate()
      .map(|(i, def)| (def, sccs.scc(i)))
      .collect();
  }

  pub(crate) fn cyclic_edge(&self, from: DefId, to: DefId) -> bool {
    self.prepare(from);
    let components = self.components.borrow();
    components.get(&from) == components.get(&to)
  }

  pub(crate) fn summary(
    self: &Rc<Self>,
    key: Instance<'tcx>,
  ) -> Option<Rc<CalleeSummary<'tcx>>> {
    if let Some(summary) = self.summaries.borrow().get(&key) {
      self.stats.borrow_mut().cache_hits += 1;
      return summary.clone();
    }
    // This is an invariant check, not a traversal-order dependent cycle policy.
    assert!(
      self.active.borrow_mut().insert(key.def_id()),
      "cycle escaped SCC classification"
    );
    let before = self.stats.borrow().construction_time;
    let start = std::time::Instant::now();
    let summary = summary::compute(self.clone(), self.tcx, key.def_id());
    let elapsed = start.elapsed();
    self.active.borrow_mut().remove(&key.def_id());
    let summary = summary.map(Rc::new);
    self.summaries.borrow_mut().insert(key, summary.clone());
    let mut stats = self.stats.borrow_mut();
    stats.computations += 1;
    stats.construction_time = before + elapsed;
    log::debug!(
      "Summary {}: {:?}",
      self.tcx.def_path_str(key.def_id()),
      elapsed
    );
    summary
  }
}

impl Drop for AnalysisSession<'_> {
  fn drop(&mut self) {
    if self.stats.get_mut().computations > 0 {
      log::info!("Callee summaries: {:?}", self.stats.get_mut());
    }
  }
}
