//! One call-effects path for propagation, summarization and source slicing.

use std::rc::Rc;

use indexical::{
  IndexedValue,
  bitset::rustc::{IndexMatrix, IndexSet},
};
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::TyKind,
};
use rustc_utils::{BodyExt, OperandExt, PlaceExt};

use super::{
  analysis::FlowAnalysis,
  mutation::{ModularMutationVisitor, Mutation, MutationStatus},
};
use crate::{extensions::ContextMode, mir::utils};

#[derive(Default)]
pub(crate) struct CallEffects<'tcx> {
  pub mutations: Vec<Mutation<'tcx>>,
  // These are values consumed by the call, even when there is no result/write.
  pub reads: Vec<Place<'tcx>>,
}

impl<'tcx> FlowAnalysis<'_, 'tcx> {
  /// Visits the effects used by this analysis, including read-only calls.
  ///
  /// Consumers should use this instead of independently approximating call
  /// mutations from signatures, which would discard `Recurse` precision.
  pub fn visit_effects(
    &self,
    mut visit: impl FnMut(Location, &[Mutation<'tcx>], &[Place<'tcx>]),
  ) {
    if self.session.mode().context_mode == ContextMode::SigOnly {
      ModularMutationVisitor::new(&self.place_info, |loc, mts| visit(loc, &mts, &[]))
        .visit_body(self.body);
      return;
    }
    for location in self.body.all_locations() {
      match self.body.stmt_at(location) {
        either::Either::Left(statement) => {
          visit(location, &self.statement_mutations(statement, location), &[
          ])
        }
        either::Either::Right(terminator) => {
          let effects = self.effects_at(terminator, location);
          visit(location, &effects.mutations, &effects.reads);
        }
      }
    }
  }

  pub(crate) fn inputs_deps<D: IndexedValue + 'static>(
    &self,
    state: &IndexMatrix<Place<'tcx>, D>,
    inputs: &[Place<'tcx>],
  ) -> IndexSet<D> {
    let mut deps = IndexSet::new(state.col_domain());
    for input in inputs {
      for relevant in self.influences(*input) {
        deps.union(state.row_set(&self.place_info.normalize(relevant)));
      }
    }
    deps
  }

  pub(crate) fn statement_mutations(
    &self,
    statement: &Statement<'tcx>,
    location: Location,
  ) -> Vec<Mutation<'tcx>> {
    let mut mutations = Vec::new();
    ModularMutationVisitor::new(&self.place_info, |_, found| mutations = found)
      .visit_statement(statement, location);
    if self.session.mode().context_mode == ContextMode::Recurse
      && let StatementKind::Assign(assign) = &statement.kind
      && let (_, Rvalue::Ref(_, _, borrowed)) = &**assign
    {
      // Taking an address is not reading the contents of the pointee. Preserve
      // the pointers and indexes used to form the address, not sibling fields.
      let inputs = borrowed
        .refs_in_projection(self.body, self.tcx)
        .map(|(p, _)| Place::from_ref(p, self.tcx))
        .chain(borrowed.projection.iter().filter_map(|elem| match elem {
          ProjectionElem::Index(local) => Some(Place::from(local)),
          _ => None,
        }))
        .collect::<Vec<_>>();
      for mutation in &mut mutations {
        mutation.inputs.clone_from(&inputs);
      }
    }
    mutations
  }

  pub(crate) fn effects_at(
    &self,
    terminator: &Terminator<'tcx>,
    location: Location,
  ) -> Rc<CallEffects<'tcx>> {
    if let Some(effects) = self.call_effects.borrow().get(&location) {
      return effects.clone();
    }
    let effects = self.precise_call_effects(terminator).unwrap_or_else(|| {
      let mut effects = CallEffects::default();
      ModularMutationVisitor::new(&self.place_info, |_, mutations| {
        effects.mutations = mutations;
      })
      .visit_terminator(terminator, location);
      match &terminator.kind {
        TerminatorKind::Call { args, .. } => {
          // A unit return doesn't mean an unknown function didn't read inputs.
          for (_, arg) in utils::arg_places(args) {
            effects.reads.push(arg);
            effects
              .reads
              .extend(self.place_info.reachable_values(arg, Mutability::Not));
          }
          // Recurse-mode borrows carry address provenance, not pointee contents.
          // An opaque call must explicitly read the reachable values for *each
          // output effect* as well, or e.g. Vec::len would lose its Vec inputs.
          for mutation in &mut effects.mutations {
            mutation.inputs = mutation
              .inputs
              .iter()
              .flat_map(|arg| {
                self
                  .place_info
                  .reachable_values(*arg, Mutability::Not)
                  .iter()
                  .copied()
                  .chain(std::iter::once(*arg))
              })
              .collect();
          }
        }
        TerminatorKind::Drop { place, .. } => {
          // Destructors are opaque calls in v1. Include reachable borrowed state.
          effects.reads.push(*place);
          effects
            .reads
            .extend(self.place_info.reachable_values(*place, Mutability::Not));
          for place in self.place_info.reachable_values(*place, Mutability::Mut) {
            effects.mutations.push(Mutation {
              mutated: *place,
              inputs: effects.reads.clone(),
              status: MutationStatus::Possibly,
            });
          }
        }
        TerminatorKind::SwitchInt { discr, .. } => effects.reads.extend(discr.as_place()),
        TerminatorKind::Assert { cond, .. } => effects.reads.extend(cond.as_place()),
        _ => {}
      }
      effects
    });
    let effects = Rc::new(effects);
    self
      .call_effects
      .borrow_mut()
      .insert(location, effects.clone());
    effects
  }

  fn precise_call_effects(
    &self,
    terminator: &Terminator<'tcx>,
  ) -> Option<CallEffects<'tcx>> {
    let TerminatorKind::Call {
      func,
      args,
      destination,
      unwind,
      ..
    } = &terminator.kind
    else {
      return None;
    };
    let key = self.session.resolve(self.def_id, func)?;
    if self.session.cyclic_edge(self.def_id, key.def_id()) {
      self.session.fallback("recursive edge");
      return None;
    }
    let summary = self.session.summary(key)?;
    let child_body = &self.session.body(key.def_id()).body;
    let actuals = utils::arg_places(args);
    let translate = |child: Place<'tcx>| -> Option<(Place<'tcx>, bool)> {
      let parent = if child.local == RETURN_PLACE {
        *destination
      } else {
        actuals
          .iter()
          .find(|(i, _)| *i + 1 == child.local.as_usize())?
          .1
      };
      let mut projection = parent.projection.to_vec();
      let mut ty = parent.ty(self.body, self.tcx);
      for elem in child.projection.iter() {
        // Widen at inaccessible fields, preserving the existing privacy boundary.
        if let ProjectionElem::Field(field, _) = elem
          && let Some(adt) = ty.ty.ty_adt_def()
          && !adt
            .variant(ty.variant_index.unwrap_or(rustc_abi::FIRST_VARIANT))
            .fields[field]
            .vis
            .is_accessible_from(self.def_id, self.tcx)
        {
          break;
        }
        // A callee's index local has no meaning in its caller. Widen arrays and
        // opaque casts rather than exporting that local into another MIR body.
        if !matches!(
          elem,
          ProjectionElem::Deref
            | ProjectionElem::Field(..)
            | ProjectionElem::Downcast(..)
        ) {
          break;
        }
        // Field MIR types belong to the generic callee. Recompute them from the
        // actual caller type before projecting (Wrap<T>::field may be an i32).
        let elem = match elem {
          ProjectionElem::Field(field, _) => {
            let field_ty = match ty.ty.kind() {
              TyKind::Adt(adt, args) => adt
                .variant(ty.variant_index.unwrap_or(rustc_abi::FIRST_VARIANT))
                .fields[field]
                .ty(self.tcx, args),
              TyKind::Tuple(fields) => fields[field.as_usize()],
              _ => break,
            };
            ProjectionElem::Field(field, field_ty)
          }
          elem => elem,
        };
        ty = ty.projection_ty(self.tcx, elem);
        projection.push(elem);
      }
      let widened = projection.len() < parent.projection.len() + child.projection.len();
      Some((Place::make(parent.local, &projection, self.tcx), widened))
    };
    let translate_input = |child: Place<'tcx>| -> Vec<Place<'tcx>> {
      let Some((parent, _)) = translate(child) else {
        return Vec::new();
      };
      let ty = child.ty(child_body, self.tcx).ty;
      if ty.is_ref() || ty.is_raw_ptr() {
        // An address origin must not acquire all of its pointee's contents.
        vec![parent]
      } else {
        // A symbolic T or opaque aggregate can hide borrowed contents that
        // become visible in the caller. Widen value origins at that boundary.
        self
          .place_info
          .reachable_values(parent, Mutability::Not)
          .iter()
          .copied()
          .chain(std::iter::once(parent))
          .collect()
      }
    };
    let mut effects = CallEffects::default();
    effects.reads = summary
      .reads
      .iter()
      .flat_map(|p| translate_input(*p))
      .collect();
    let widened_return = summary
      .writes
      .iter()
      .filter(|effect| effect.place.local == RETURN_PLACE)
      .any(|effect| translate(effect.place).is_some_and(|(_, widened)| widened));
    for effect in &summary.writes {
      let Some((mutated, _)) = translate(effect.place) else {
        continue;
      };
      // A constant actual argument has no caller provenance. Its projections
      // cannot name mutable caller state, either.
      effects.mutations.push(Mutation {
        mutated,
        inputs: effect
          .inputs
          .iter()
          .flat_map(|p| translate_input(*p))
          .collect(),
        // Widened fields can overlap other output fields. A strong update of an
        // ancestor must not erase another effect from this same call.
        status: if effect.place.local == RETURN_PLACE
          && !widened_return
          && !matches!(unwind, UnwindAction::Cleanup(_))
        {
          MutationStatus::Definitely
        } else {
          MutationStatus::Possibly
        },
      });
    }
    Some(effects)
  }
}
