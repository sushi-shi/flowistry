//! The effects of instructions in
//! [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse): calls are
//! analyzed with callee summaries when possible, and with the modular approximation
//! otherwise.

use std::rc::Rc;

use rustc_data_structures::fx::FxHashSet;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{TyCtxt, TyKind, TypingEnv},
};

use super::{
  analysis::FlowAnalysis,
  callsite::{FallbackReason, cmp_places_structurally},
  mutation::{ModularMutationVisitor, Mutation, MutationKind},
};
use crate::{extensions::REACHED_LIBRARY, mir::utils::ErasedTy};

/// The effects of a terminator.
#[derive(Debug, Default)]
pub(crate) struct CallEffects<'tcx> {
  /// All the mutations of the terminator.
  pub mutations: Vec<Mutation<'tcx>>,
}

impl<'tcx> FlowAnalysis<'_, 'tcx> {
  /// The mutations of a statement.
  pub(crate) fn statement_mutations(
    &self,
    statement: &Statement<'tcx>,
    location: Location,
  ) -> Vec<Mutation<'tcx>> {
    let mut mutations = Vec::new();
    ModularMutationVisitor::new(&self.place_info, |_, found| mutations.extend(found))
      .visit_statement(statement, location);
    mutations
  }

  /// The effects of a terminator in `Recurse` mode (cached): a call is analyzed with
  /// a summary of its callee if possible.
  pub(crate) fn effects_at(
    &self,
    terminator: &Terminator<'tcx>,
    location: Location,
  ) -> Rc<CallEffects<'tcx>> {
    if let Some(effects) = self.call_effects.borrow().get(&location) {
      return effects.clone();
    }
    let effects = match &terminator.kind {
      TerminatorKind::Call { .. } => match self.recurse_into_call(&terminator.kind) {
        Ok(effects) => effects,
        Err(reason) => {
          log::debug!("  Not recursing into call: {reason:?}");
          self.session.record_fallback(reason);
          if reason == FallbackReason::NotLocal {
            REACHED_LIBRARY.get(|reached_library| {
              if let Some(reached_library) = reached_library {
                *reached_library.borrow_mut() = true;
              }
            });
          }
          self.modular_effects(terminator, location)
        }
      },
      TerminatorKind::Drop { place, .. } => self.drop_effects(*place),
      _ => self.modular_effects(terminator, location),
    };
    let effects = Rc::new(effects);
    self
      .call_effects
      .borrow_mut()
      .insert(location, effects.clone());
    effects
  }

  /// The modular approximation of the effects of a terminator.
  fn modular_effects(
    &self,
    terminator: &Terminator<'tcx>,
    location: Location,
  ) -> CallEffects<'tcx> {
    let mut effects = CallEffects::default();
    ModularMutationVisitor::new(&self.place_info, |_, mutations| {
      effects.mutations = mutations;
    })
    .visit_terminator(terminator, location);
    effects
  }

  /// The effects of dropping `place`: if its drop glue may run a destructor of the
  /// source code, that destructor may write whatever is mutably reachable from
  /// `place`, e.g. through a `&mut` held by a guard.
  fn drop_effects(&self, place: Place<'tcx>) -> CallEffects<'tcx> {
    let tcx = self.tcx;
    let typing_env = TypingEnv::post_analysis(tcx, self.def_id);
    let ty = ErasedTy::new(tcx, place.ty(self.body.local_decls(), tcx).ty);
    if !drop_may_run_user_code(tcx, typing_env, ty, &mut FxHashSet::default()) {
      return CallEffects::default();
    }
    let mut reachable = self
      .place_info
      .reachable_values(place, Mutability::Mut)
      .iter()
      .copied()
      .collect::<Vec<_>>();
    reachable.sort_by(|p1, p2| {
      cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
    });
    let mutations = reachable
      .into_iter()
      .map(|mutated| Mutation {
        mutated,
        inputs: vec![place],
        kind: MutationKind::Destructor,
      })
      .collect();
    CallEffects { mutations }
  }
}

/// Whether dropping a value of type `ty` may run a destructor that is not part of
/// the standard library (whose destructors only drop the values of their type
/// parameters, e.g. the elements of a `Vec`, or release resources).
///
/// Types whose destructor is unknown (trait objects, type parameters, ...) may.
fn drop_may_run_user_code<'tcx>(
  tcx: TyCtxt<'tcx>,
  typing_env: TypingEnv<'tcx>,
  ty: ErasedTy<'tcx>,
  visited: &mut FxHashSet<ErasedTy<'tcx>>,
) -> bool {
  if !visited.insert(ty) || !ty.needs_drop(tcx, typing_env) {
    return false;
  }
  let mut any = |tys: &mut dyn Iterator<Item = rustc_middle::ty::Ty<'tcx>>| {
    for ty in tys {
      if drop_may_run_user_code(tcx, typing_env, ErasedTy::new(tcx, ty), visited) {
        return true;
      }
    }
    false
  };
  match ty.ty().kind() {
    TyKind::Adt(adt_def, args) => {
      let from_std = matches!(
        tcx.crate_name(adt_def.did().krate).as_str(),
        "core" | "alloc" | "std"
      );
      if from_std {
        any(&mut args.types())
      } else if adt_def.has_dtor(tcx) {
        true
      } else {
        any(&mut adt_def.all_fields().map(|field| field.ty(tcx, args)))
      }
    }
    TyKind::Tuple(tys) => any(&mut tys.iter()),
    TyKind::Array(ty, _) | TyKind::Slice(ty) | TyKind::Pat(ty, _) => {
      any(&mut std::iter::once(*ty))
    }
    TyKind::Closure(_, args) => any(&mut args.as_closure().upvar_tys().iter()),
    _ => true,
  }
}
