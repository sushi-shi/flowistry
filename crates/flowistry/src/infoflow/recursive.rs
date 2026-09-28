//! Interprocedural analysis: in
//! [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse), a call to a
//! local function is analyzed by computing the flow of the callee's body and
//! translating its effects on the return place and on pointed-to arguments into
//! mutations of the caller.

use log::{debug, info};
use rustc_middle::{
  mir::*,
  ty::{ClosureKind, GenericArgKind, TyKind},
};
use rustc_mir_dataflow::JoinSemiLattice;
use rustc_utils::mir::{
  borrowck_facts::get_body_with_borrowck_facts, location_or_arg::index::LocationOrArgSet,
};
use smallvec::{SmallVec, smallvec};

use super::{
  BODY_STACK, FlowResults,
  analysis::FlowAnalysis,
  callsite::{
    CallSite, CalleeRow, Coarsening, EffectPath, FallbackReason, Resolved, RowRole,
    Target,
  },
};
use crate::{
  infoflow::{
    FlowDomain,
    mutation::{CalleeEffect, Mutation, MutationKind, Precision},
  },
  mir::utils::{self, ErasedTy},
};

/// The state of a callee at its exits (the join of its states at every `return`).
///
/// Its rows are places of the callee body, readable only as [`CalleeRow`]s, so they
/// cannot be confused with rows of the caller's state.
struct CalleeExitState<'tcx> {
  state: FlowDomain<'tcx>,
}

impl<'tcx> CalleeExitState<'tcx> {
  fn new(flow: &FlowResults<'_, 'tcx>, body: &Body<'tcx>) -> Self {
    let mut state = FlowDomain::new(flow.analysis.location_domain());
    let return_locs = body
      .basic_blocks
      .iter_enumerated()
      .filter_map(|(bb, data)| match data.terminator().kind {
        TerminatorKind::Return => Some(body.terminator_loc(bb)),
        _ => None,
      });
    for loc in return_locs {
      state.join(flow.state_at(loc));
    }
    CalleeExitState { state }
  }

  fn rows(&self) -> impl Iterator<Item = (CalleeRow<'tcx>, &LocationOrArgSet)> + '_ {
    self
      .state
      .rows()
      .map(|(row, deps)| (CalleeRow::new(*row), deps))
  }

  fn deps(&self, row: CalleeRow<'tcx>) -> &LocationOrArgSet {
    self.state.row_set(&row.key())
  }

  /// Whether the callee may have written the row.
  ///
  /// Arguments always depend on their own synthetic location, so a row with more than
  /// one dependency was written. FIXME: a row written with only its own argument as
  /// a dependency is considered unwritten.
  fn written_in_callee(&self, row: CalleeRow<'tcx>) -> bool {
    self.deps(row).len() > 1
  }
}

/// What the caller learns from one row of the callee's exit state.
///
/// This is the export policy of the interprocedural analysis, decided by [`export`].
enum RowExport {
  /// The row is not observable by the caller.
  Hidden,
  /// The caller place of the row can be an input of callee effects, but the callee
  /// did not write it (or its writes are invisible to the caller).
  Source(EffectPath),
  /// The callee wrote the row, which the caller observes as the given kind of
  /// effect (once the precision of its translation is known).
  Effect(EffectPath, fn(Precision) -> CalleeEffect),
}

impl RowExport {
  fn source(&self) -> Option<&EffectPath> {
    match self {
      RowExport::Hidden => None,
      RowExport::Source(path) | RowExport::Effect(path, _) => Some(path),
    }
  }
}

/// The export policy: which callee rows become caller effects or inputs.
fn export(role: RowRole, row_ty: ErasedTy<'_>, written: bool) -> RowExport {
  // Unit rows carry no data.
  if row_ty.is_unit() {
    return RowExport::Hidden;
  }
  match role {
    RowRole::Internal => RowExport::Hidden,
    // The return place is always written by the time the callee returns.
    RowRole::Return(path) => RowExport::Effect(path, CalleeEffect::Return),
    RowRole::ArgPointee(path) if written => {
      RowExport::Effect(path, CalleeEffect::ArgPointee)
    }
    RowRole::ArgPointee(path) => RowExport::Source(path),
    // Writes to a parameter itself are local to the callee.
    RowRole::ArgDirect(path) => RowExport::Source(path),
  }
}

impl<'tcx> FlowAnalysis<'_, 'tcx> {
  /// Computes the mutations of a call by analyzing the callee, or says why the call
  /// must be analyzed with the modular approximation instead.
  pub(crate) fn recurse_into_call(
    &self,
    call: &TerminatorKind<'tcx>,
  ) -> Result<Vec<Mutation<'tcx>>, FallbackReason> {
    let tcx = self.tcx;
    let TerminatorKind::Call { func, args, .. } = call else {
      return Err(FallbackReason::NotACall);
    };
    debug!("Checking whether can recurse into {func:?}");

    let func = func.constant().ok_or(FallbackReason::FuncNotConstant)?;
    let TyKind::FnDef(def_id, _) = func.const_.ty().kind() else {
      return Err(FallbackReason::NotFnDef);
    };
    let def_id = *def_id;

    // If a function returns never (fn () -> !) then there are no exit points,
    // so we can't analyze effects on exit
    let fn_sig = tcx.fn_sig(def_id);
    if fn_sig.skip_binder().output().skip_binder().is_never() {
      return Err(FallbackReason::ReturnsNever);
    }

    let node = tcx
      .hir_get_if_local(def_id)
      .ok_or(FallbackReason::NotLocal)?;
    let body_id = node.body_id().ok_or(FallbackReason::NoBody)?;

    // TODO(wcrichto, 2024-12-02): mir_unsafety_check_result got removed, need to find a replacement
    // let unsafety = tcx.mir_unsafety_check_result(def_id.expect_local());
    // if !unsafety.used_unsafe_blocks.is_empty() {
    //   debug!("  Func contains unsafe blocks");
    //   return false;
    // }

    let any_closure_inputs = utils::arg_places(args).iter().any(|(_, place)| {
      let ty = place.ty(self.body.local_decls(), tcx).ty;
      ty.walk().any(|arg| match arg.kind() {
        GenericArgKind::Type(ty) => match ty.kind() {
          TyKind::Closure(_, substs) => matches!(
            substs.as_closure().kind(),
            ClosureKind::FnOnce | ClosureKind::FnMut
          ),
          _ => false,
        },
        _ => false,
      })
    });
    if any_closure_inputs {
      return Err(FallbackReason::FnMutOrOnceClosureArg);
    }

    let recursive = BODY_STACK.with(|body_stack| body_stack.borrow().contains(&body_id));
    if recursive {
      return Err(FallbackReason::RecursiveCall);
    }

    let body_with_facts = get_body_with_borrowck_facts(tcx, def_id.expect_local());
    let body = &body_with_facts.body;
    let site = CallSite::parse(tcx, self.def_id, self.body, call, def_id, body)?;

    let mut recurse_cache = self.recurse_cache.borrow_mut();
    let flow = recurse_cache.entry(body_id).or_insert_with(|| {
      info!("Recursing into {}", tcx.def_path_debug_str(def_id));
      super::compute_flow_with_mode(tcx, body_id, body_with_facts, self.place_info.mode())
    });
    let exit = CalleeExitState::new(flow, body);

    let rows = exit
      .rows()
      .map(|(row, _)| {
        let role = site.classify(row);
        let export = export(role, row.ty(body, tcx), exit.written_in_callee(row));
        (row, export)
      })
      .collect::<Vec<_>>();

    // Translate every observable row once. Rows without caller state (e.g. rooted at
    // a constant operand) are neither effects nor inputs.
    let translated = rows
      .iter()
      .filter_map(|(row, export)| {
        let path = export.source()?;
        match site.translate(path) {
          Resolved::Target(target) => Some((*row, export, target)),
          Resolved::NoCallerState => None,
        }
      })
      .collect::<Vec<_>>();

    // Emit parents before their children, in a deterministic order.
    let mut effects = translated
      .iter()
      .filter_map(|(row, export, target)| match export {
        RowExport::Effect(path, effect) => Some((path, *effect, *row, *target)),
        RowExport::Hidden | RowExport::Source(_) => None,
      })
      .collect::<Vec<_>>();
    // Ties (rows with the same path) are broken structurally, so the order is total.
    effects.sort_by(|(p1, _, r1, _), (p2, _, r2, _)| {
      (p1.elems.len(), p1)
        .cmp(&(p2.elems.len(), p2))
        .then_with(|| r1.cmp_structural(*r2))
    });

    let mutations = effects
      .into_iter()
      .flat_map(|(_, effect, row, target)| {
        let row_deps = exit.deps(row);
        let inputs = translated
          .iter()
          .filter(|(source, ..)| row_deps.is_superset(exit.deps(*source)))
          .map(|(_, _, source)| source.place())
          .collect::<Vec<_>>();

        let precision = match target {
          Target::Exact(_) => Precision::Exact,
          Target::Coarsened { .. } => Precision::Coarsened,
        };
        let effect = effect(precision);
        debug!("callee row {row:?} -> {target:?}, inputs {inputs:?}");

        self
          .write_targets(target)
          .into_iter()
          .map(|mutated| Mutation {
            mutated,
            inputs: inputs.clone(),
            kind: MutationKind::CalleeEffect(effect),
          })
          .collect::<SmallVec<[Mutation<'tcx>; 4]>>()
      })
      .collect::<Vec<_>>();

    Ok(mutations)
  }

  /// The caller places written by a callee effect on `target`.
  fn write_targets(&self, target: Target<'tcx>) -> SmallVec<[Place<'tcx>; 4]> {
    match target {
      Target::Exact(place) => smallvec![place],
      Target::Coarsened {
        place,
        lost: Coarsening::Interior,
      } => smallvec![place],
      // The effect is behind a pointer stored somewhere in `place` that the caller
      // cannot name: it may write anything mutably reachable from `place`.
      Target::Coarsened {
        place,
        lost: Coarsening::ThroughPointer,
      } => {
        let mut targets: SmallVec<[Place<'tcx>; 4]> = smallvec![place];
        let reachable = self.place_info.reachable_values(place, Mutability::Mut);
        let mut reachable = reachable
          .iter()
          .copied()
          .filter(|reachable| *reachable != place)
          .collect::<SmallVec<[_; 4]>>();
        // Deterministic order.
        reachable.sort_by_cached_key(|reachable| format!("{reachable:?}"));
        targets.extend(reachable);
        targets
      }
    }
  }
}
