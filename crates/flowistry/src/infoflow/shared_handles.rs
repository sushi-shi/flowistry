//! Separately held handles that may share interior-mutable state.
//!
//! Lifetimes connect a `RefCell` guard to the handle it was borrowed from, but
//! nothing connects two `Rc<RefCell<T>>` values that point to the same object.
//! Whether two handles are distinct cannot be decided in general, so this module
//! groups the handles of a body by the type of their shared state. A second, more
//! pessimistic flow analysis (see
//! [`compute_flow_with_shared_handles`](super::compute_flow_with_shared_handles))
//! lets a write to the state of one handle possibly write the state of every other
//! handle of its group.

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::{
  mir::{Place, ProjectionElem},
  ty::TypingEnv,
};
use rustc_utils::PlaceExt;

use super::{
  interior::Handle,
  mutation::{CalleeEffect, Mutation, MutationKind},
};
use crate::mir::{
  placeinfo::{NormPlace, PlaceInfo},
  utils::ErasedTy,
};

/// The handles of a body that may share interior-mutable state, grouped by the type
/// of that state.
pub(crate) struct SharedHandles<'tcx> {
  /// The (normalized) place standing for the state of each handle, and its group.
  /// A reference stands for its pointee; an `Rc`/`Arc` stands for itself, as its
  /// pointee has no place of its own.
  handles: Vec<(NormPlace<'tcx>, usize)>,
  groups: Vec<Vec<NormPlace<'tcx>>>,
}

impl<'tcx> SharedHandles<'tcx> {
  /// Groups the handles of the body of `place_info`. Returns `None` unless at least
  /// two handles share a state type, in which case the pessimistic analysis would
  /// equal the exact one.
  pub fn build(place_info: &PlaceInfo<'_, 'tcx>) -> Option<Self> {
    let (tcx, body, def_id) = (place_info.tcx, place_info.body, place_info.def_id);
    let typing_env = TypingEnv::post_analysis(tcx, def_id);
    let locals = body
      .local_decls
      .indices()
      .flat_map(|local| Place::from_local(local, tcx).interior_places(tcx, body, def_id));
    let candidates = locals.chain(place_info.all_args().map(|(place, _)| place));

    let mut groups: Vec<Vec<NormPlace<'tcx>>> = Vec::new();
    let mut group_of: FxHashMap<ErasedTy<'tcx>, usize> = FxHashMap::default();
    for place in candidates {
      let ty = ErasedTy::new(tcx, place.ty(&body.local_decls, tcx).ty);
      let Some(handle) = Handle::parse(tcx, typing_env, ty) else {
        continue;
      };
      let state = match handle {
        Handle::Ref { .. } => tcx.mk_place_deref(place),
        Handle::Owning { .. } => place,
      };
      let state = place_info.normalize(state);
      let group = *group_of.entry(handle.pointee()).or_insert_with(|| {
        groups.push(Vec::new());
        groups.len() - 1
      });
      if !groups[group].contains(&state) {
        groups[group].push(state);
      }
    }

    let groups = groups
      .into_iter()
      .filter(|members| members.len() > 1)
      .collect::<Vec<_>>();
    let handles = groups
      .iter()
      .enumerate()
      .flat_map(|(group, members)| members.iter().map(move |place| (*place, group)))
      .collect::<Vec<_>>();
    (!handles.is_empty()).then_some(SharedHandles { handles, groups })
  }

  /// The states of the other handles that the write of `mutation` to its alias
  /// `alias` may also write.
  pub fn possibly_shared(
    &self,
    mutation: &Mutation<'tcx>,
    alias: Place<'tcx>,
    place_info: &PlaceInfo<'_, 'tcx>,
  ) -> impl Iterator<Item = NormPlace<'tcx>> + '_ {
    // Writing a new value into a handle (e.g. `b = a.clone()`, or a call returning
    // one) rebinds it, it does not write through it.
    let rebinds = alias == mutation.mutated && writes_value(mutation.kind);
    let alias = place_info.normalize(alias);
    self
      .handles
      .iter()
      .filter(move |(handle, _)| !rebinds && writes_state(*handle, alias))
      .flat_map(move |(handle, group)| {
        self.groups[*group]
          .iter()
          .copied()
          .filter(move |other| other != handle)
      })
  }
}

/// Whether a mutation of this kind writes a new value into the mutated place itself,
/// as opposed to a write through it, or through what it gives access to.
fn writes_value(kind: MutationKind) -> bool {
  match kind {
    MutationKind::Assign
    | MutationKind::CallReturn
    | MutationKind::AsmOutput
    | MutationKind::CalleeEffect(CalleeEffect::Return(_)) => true,
    MutationKind::CallArgument { .. }
    | MutationKind::AsmMemory { .. }
    | MutationKind::Destructor
    | MutationKind::CalleeEffect(
      CalleeEffect::ArgPointee(_) | CalleeEffect::SharedState(_),
    ) => false,
  }
}

/// Whether writing `mutated` may write the shared `state`: it is inside the state,
/// or it contains the state without a pointer in between (e.g. `app` for the state
/// `app.rng` of an `Rc`). Rewriting a reference is not writing its pointee.
fn writes_state<'tcx>(state: NormPlace<'tcx>, mutated: NormPlace<'tcx>) -> bool {
  if state.local() != mutated.local() {
    return false;
  }
  let (state, mutated) = (state.projection(), mutated.projection());
  if mutated.starts_with(state) {
    return true;
  }
  state.starts_with(mutated) && !state[mutated.len() ..].contains(&ProjectionElem::Deref)
}
