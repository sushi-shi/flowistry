//! Interprocedural analysis: in
//! [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse), a call to a
//! local function is analyzed with a [summary](super::summary) of the callee, whose
//! effects on the return place and on pointed-to arguments are translated into
//! mutations of the caller.

use log::debug;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::{
  mir::*,
  ty::{ClosureKind, Instance, InstanceKind, TyCtxt, TyKind, TypingEnv},
};
use rustc_span::Spanned;
use smallvec::{SmallVec, smallvec};

use super::{
  analysis::FlowAnalysis,
  callsite::{
    CallSite, Coarsening, FallbackReason, Resolved, Target, cmp_places_structurally,
  },
  effects::CallEffects,
  mutation::{CalleeEffect, Mutation, MutationKind, Precision, call_argument_writes},
  summary::{CalleeSummary, InputContents},
};

/// Resolves the function called by `func` from `caller` to a local body that can be
/// summarized, or says why the call must be analyzed with the modular approximation.
///
/// The call graph of a [session](super::AnalysisSession) is built with this function,
/// so every call that is analyzed with a summary is an edge of that graph.
pub(crate) fn resolve_callee<'tcx>(
  tcx: TyCtxt<'tcx>,
  caller: LocalDefId,
  func: &Operand<'tcx>,
) -> Result<LocalDefId, FallbackReason> {
  let func = func.constant().ok_or(FallbackReason::FuncNotConstant)?;
  let TyKind::FnDef(def_id, fn_args) = func.const_.ty().kind() else {
    return Err(FallbackReason::NotFnDef);
  };

  // A call names an item, e.g. a trait method or `Fn::call`. The body that runs is
  // the one it resolves to, e.g. an impl's method or a closure body. Only a static
  // call to an item of the source code has such a body: a virtual call through
  // `dyn Trait` (InstanceKind::Virtual), a shim or an intrinsic does not.
  let typing_env = TypingEnv::post_analysis(tcx, caller);
  let fn_args = tcx.erase_and_anonymize_regions(*fn_args);
  let instance = match Instance::try_resolve(tcx, typing_env, *def_id, fn_args) {
    Ok(Some(instance)) => instance,
    Ok(None) | Err(_) => return Err(FallbackReason::Unresolved),
  };
  let InstanceKind::Item(resolved) = instance.def else {
    return Err(FallbackReason::NotAnItem);
  };

  let mutable_closure_arg = instance.args.types().any(|ty| {
    ty.walk()
      .filter_map(|arg| arg.as_type())
      .any(|ty| match ty.kind() {
        TyKind::Closure(_, args) => matches!(
          args.as_closure().kind(),
          ClosureKind::FnMut | ClosureKind::FnOnce
        ),
        _ => false,
      })
  });
  if mutable_closure_arg {
    return Err(FallbackReason::FnMutOrOnceClosureArg);
  }

  let resolved = resolved.as_local().ok_or(FallbackReason::NotLocal)?;
  tcx
    .hir_get_if_local(resolved.to_def_id())
    .and_then(|node| node.body_id())
    .ok_or(FallbackReason::NoBody)?;
  Ok(resolved)
}

impl<'tcx> FlowAnalysis<'_, 'tcx> {
  /// Computes the effects of a call from a summary of the callee, or says why the
  /// call must be analyzed with the modular approximation instead.
  pub(crate) fn recurse_into_call(
    &self,
    call: &TerminatorKind<'tcx>,
  ) -> Result<CallEffects<'tcx>, FallbackReason> {
    let TerminatorKind::Call { func, args, .. } = call else {
      return Err(FallbackReason::NotACall);
    };
    debug!("Checking whether can recurse into {func:?}");
    let caller = self.def_id.expect_local();
    let callee = resolve_callee(self.tcx, caller, func)?;
    if self.session.same_component(caller, callee) {
      return Err(FallbackReason::RecursiveCall);
    }
    // A callee that never returns has no normal exit, and the call no destination.
    if self.session.body(callee).body.return_ty().is_never() {
      return Err(FallbackReason::ReturnsNever);
    }
    let summary = self.session.summary(callee)?;
    let site = CallSite::new(
      self.tcx,
      self.def_id,
      self.body,
      call,
      callee.to_def_id(),
      summary.abi,
    )?;
    Ok(self.instantiate(&summary, &site, args))
  }

  /// The effects of a call described by the callee's summary.
  fn instantiate(
    &self,
    summary: &CalleeSummary,
    site: &CallSite<'_, 'tcx>,
    args: &[Spanned<Operand<'tcx>>],
  ) -> CallEffects<'tcx> {
    // The caller places of each origin, i.e. of each parameter place the callee
    // may read. Origins without caller state (e.g. rooted at a constant operand)
    // have no caller places.
    let origin_places = summary
      .origins
      .iter()
      .map(|origin| match site.translate(&origin.path) {
        Resolved::Target(target) => self.input_places(target, origin.contents),
        Resolved::NoCallerState => SmallVec::new(),
      })
      .collect::<Vec<_>>();
    let inputs_of = |origins: &[usize]| -> Vec<Place<'tcx>> {
      let mut inputs = origins
        .iter()
        .flat_map(|origin| origin_places[*origin].iter().copied())
        .collect::<Vec<_>>();
      inputs.dedup();
      inputs
    };

    // The callee always writes its whole return place. This write comes first: the
    // effects on the parts of the return value then refine it. Its inputs are those
    // of the unit parts of the return value only (e.g. `Ok(())` depending on a
    // condition), so that each data field keeps its own dependencies.
    let whole_return = Mutation {
      mutated: site.destination(),
      inputs: inputs_of(&summary.whole_return_inputs),
      kind: MutationKind::CalleeEffect(CalleeEffect::Return(Precision::Exact)),
    };

    // Effects are ordered in the summary: parents before their children.
    let effect_mutations = summary.effects.iter().flat_map(|effect| {
      let target = match site.translate(&effect.path) {
        Resolved::Target(target) => target,
        // A write through a pointer that has no caller state (e.g. a constant).
        Resolved::NoCallerState => return SmallVec::<[Mutation<'tcx>; 4]>::new(),
      };
      let precision = match target {
        Target::Exact(_) => Precision::Exact,
        Target::Coarsened { .. } => Precision::Coarsened,
      };
      let kind = MutationKind::CalleeEffect(effect.kind.with(precision));
      let inputs = inputs_of(&effect.inputs);
      debug!("callee effect {effect:?} -> {target:?}, inputs {inputs:?}");
      self
        .write_targets(target)
        .into_iter()
        .map(|mutated| Mutation {
          mutated,
          inputs: inputs.clone(),
          kind,
        })
        .collect()
    });

    // The modular approximation of the writes through operands passed to opaque
    // callee parameters (e.g. of a generic or trait-object type): the callee's
    // analysis cannot see the pointers hidden in them.
    let opaque_mutations = call_argument_writes(&self.place_info, args, |i| {
      summary.opaque_operands.contains(&i)
    })
    .mutations
    .into_iter()
    .map(|mutation| Mutation {
      // The callee may read through the references among the operands.
      inputs: mutation
        .inputs
        .iter()
        .flat_map(|input| self.reachable_contents(*input))
        .collect(),
      ..mutation
    });

    let mutations = std::iter::once(whole_return)
      .chain(effect_mutations)
      .chain(opaque_mutations)
      .collect();
    CallEffects {
      mutations,
      reads: inputs_of(&summary.reads),
    }
  }

  /// The caller places that stand for a parameter place read by the callee.
  fn input_places(
    &self,
    target: Target<'tcx>,
    contents: InputContents,
  ) -> SmallVec<[Place<'tcx>; 4]> {
    match (target, contents) {
      // The callee reads the pointer itself; what it reads behind it is another
      // origin.
      (Target::Exact(place), InputContents::Address)
      | (
        Target::Coarsened {
          place,
          lost: Coarsening::Interior,
        },
        InputContents::Address,
      ) => smallvec![place],
      // The callee may read what the value points to without naming it (e.g. a
      // generic value), or the path to what it read was cut above a pointer.
      (Target::Exact(place), InputContents::Reachable)
      | (Target::Coarsened { place, .. }, _) => {
        self.reachable_contents(place).into_iter().collect()
      }
    }
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
        reachable.sort_by(|p1, p2| {
          cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
        });
        targets.extend(reachable);
        targets
      }
    }
  }
}
