//! The effects of instructions in
//! [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse): calls are
//! analyzed with callee summaries when possible, and with the modular approximation
//! otherwise.
//!
//! In `Recurse` mode, a borrow `&p` reads only the pointers that form the address of
//! `p` (and its indices), not the contents of `p`: whoever reads through the
//! reference reaches `p` through its aliases. Consumers of a reference that do not
//! read through it with a place projection, and for which the aliases of the
//! reference are thus unknown, read everything reachable from it instead:
//! opaque calls (see [`FlowAnalysis::effects_at`]), destructors, and casts of
//! references to raw pointers or integers.

use std::rc::Rc;

use rustc_data_structures::fx::FxHashSet;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{TyCtxt, TyKind, TypingEnv},
};
use rustc_utils::{OperandExt, PlaceExt};

use super::{
  analysis::FlowAnalysis,
  callsite::{FallbackReason, cmp_places_structurally},
  mutation::{ModularMutationVisitor, Mutation, MutationKind},
};
use crate::{
  extensions::{ContextMode, REACHED_LIBRARY},
  mir::utils::{self, ErasedTy},
};

/// The effects of a terminator.
#[derive(Debug, Default)]
pub(crate) struct CallEffects<'tcx> {
  /// All the mutations of the terminator.
  pub mutations: Vec<Mutation<'tcx>>,
  /// The places the terminator reads, including those that none of its mutations
  /// depends on (e.g. the inputs of a call returning `()`).
  pub reads: Vec<Place<'tcx>>,
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
    if self.place_info.mode().context_mode == ContextMode::Recurse
      && let StatementKind::Assign(assign) = &statement.kind
    {
      let (_, rvalue) = &**assign;
      if let Some(inputs) = self.address_inputs(rvalue) {
        for mutation in &mut mutations {
          mutation.inputs.clone_from(&inputs);
        }
      } else if let Some(escaped) = self.escaping_reference(rvalue) {
        let reachable = self.reachable_contents(escaped);
        for mutation in &mut mutations {
          mutation.inputs.extend(reachable.iter().copied());
        }
      }
    }
    mutations
  }

  /// The inputs of a borrow: the pointers dereferenced to form the address of the
  /// borrowed place, and the locals indexing it.
  fn address_inputs(&self, rvalue: &Rvalue<'tcx>) -> Option<Vec<Place<'tcx>>> {
    let Rvalue::Ref(_, _, borrowed) = rvalue else {
      return None;
    };
    let pointers = borrowed
      .refs_in_projection(self.body, self.tcx)
      .map(|(pointer, _)| Place::from_ref(pointer, self.tcx));
    let indices = borrowed.projection.iter().filter_map(|elem| match elem {
      ProjectionElem::Index(local) => Some(Place::from_local(local, self.tcx)),
      _ => None,
    });
    Some(pointers.chain(indices).collect())
  }

  /// The reference converted by `rvalue` into a value that carries no loans (a raw
  /// pointer or an integer), if any.
  fn escaping_reference(&self, rvalue: &Rvalue<'tcx>) -> Option<Place<'tcx>> {
    let Rvalue::Cast(_, operand, target_ty) = rvalue else {
      return None;
    };
    let place = operand.as_place()?;
    let ty = place.ty(self.body.local_decls(), self.tcx).ty;
    (ty.is_ref() && !target_ty.is_ref()).then_some(place)
  }

  /// `place`, and everything reachable from it.
  pub(crate) fn reachable_contents(&self, place: Place<'tcx>) -> Vec<Place<'tcx>> {
    let mut reachable = self
      .place_info
      .reachable_values(place, Mutability::Not)
      .iter()
      .copied()
      .filter(|reachable| *reachable != place)
      .collect::<Vec<_>>();
    reachable.sort_by(|p1, p2| {
      cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
    });
    reachable.insert(0, place);
    reachable
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
          self.opaque_call_effects(terminator, location)
        }
      },
      TerminatorKind::Drop { place, .. } => self.drop_effects(*place),
      TerminatorKind::SwitchInt { discr, .. } => CallEffects {
        mutations: Vec::new(),
        reads: discr.as_place().into_iter().collect(),
      },
      TerminatorKind::Assert { cond, .. } => CallEffects {
        mutations: Vec::new(),
        reads: cond.as_place().into_iter().collect(),
      },
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
      effects.mutations.extend(mutations);
    })
    .visit_terminator(terminator, location);
    effects
  }

  /// The modular approximation of the effects of a call. The callee may read what is
  /// reachable from its operands, since it may read through the references among
  /// them.
  fn opaque_call_effects(
    &self,
    terminator: &Terminator<'tcx>,
    location: Location,
  ) -> CallEffects<'tcx> {
    let mut effects = self.modular_effects(terminator, location);
    if let TerminatorKind::Call { args, .. } = &terminator.kind {
      effects.reads = utils::arg_places(args)
        .into_iter()
        .flat_map(|(_, arg)| self.reachable_contents(arg))
        .collect();
    }
    for mutation in &mut effects.mutations {
      mutation.inputs = mutation
        .inputs
        .iter()
        .flat_map(|input| self.reachable_contents(*input))
        .collect();
    }
    effects
  }

  /// The effects of dropping `place`: its destructors may read what is reachable
  /// from it, and if its drop glue may run a destructor of the source code, that
  /// destructor may write whatever is mutably reachable from `place`, e.g. through a
  /// `&mut` held by a guard.
  fn drop_effects(&self, place: Place<'tcx>) -> CallEffects<'tcx> {
    let tcx = self.tcx;
    let typing_env = TypingEnv::post_analysis(tcx, self.def_id);
    let ty = ErasedTy::new(tcx, place.ty(self.body.local_decls(), tcx).ty);
    let reads = self.reachable_contents(place);
    if !drop_may_run_user_code(tcx, typing_env, ty, &mut FxHashSet::default()) {
      return CallEffects {
        mutations: Vec::new(),
        reads,
      };
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
        inputs: reads.clone(),
        kind: MutationKind::Destructor,
      })
      .collect();
    CallEffects { mutations, reads }
  }

  /// Calls `visit` with the mutations of each location of the body, and with what
  /// the terminators read, as this analysis computes them: in
  /// [`ContextMode::Recurse`], calls are analyzed with callee summaries.
  ///
  /// Consumers of the effects of instructions (e.g. to highlight what an
  /// instruction directly influences) should use this method instead of the
  /// modular approximation, which would discard the precision of `Recurse` mode.
  pub fn visit_effects(
    &self,
    mut visit: impl FnMut(Location, &[Mutation<'tcx>], &[Place<'tcx>]),
  ) {
    for (block, data) in self.body.basic_blocks.iter_enumerated() {
      for (statement_index, statement) in data.statements.iter().enumerate() {
        let location = Location {
          block,
          statement_index,
        };
        visit(location, &self.statement_mutations(statement, location), &[
        ]);
      }
      let location = self.body.terminator_loc(block);
      let terminator = data.terminator();
      match self.place_info.mode().context_mode {
        ContextMode::Recurse => {
          let effects = self.effects_at(terminator, location);
          visit(location, &effects.mutations, &effects.reads);
        }
        ContextMode::SigOnly => {
          let effects = self.modular_effects(terminator, location);
          visit(location, &effects.mutations, &[]);
        }
      }
    }
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
