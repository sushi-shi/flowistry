//! Separately held handles that may share interior-mutable state.
//!
//! Lifetimes connect a `RefCell` guard to the handle it was borrowed from, but
//! nothing connects two `Rc<RefCell<T>>` values that point to the same object.
//! Proving that two handles are distinct is not possible in general, so this
//! module groups handles by their pointee type. A second, more pessimistic flow
//! analysis lets a write to one handle possibly write every handle in its group.

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::{
  mir::{Mutability, Place, ProjectionElem},
  ty::{self, Ty, TyCtxt, TypingEnv},
};
use rustc_utils::PlaceExt;

use crate::mir::placeinfo::PlaceInfo;

/// Returns the pointee of a shared handle through which state may be mutated:
/// `Rc<T>`, `Arc<T>`, their `Weak`s, or `&T`, where `T` is not `Freeze`.
pub fn interior_pointee<'tcx>(
  tcx: TyCtxt<'tcx>,
  typing_env: TypingEnv<'tcx>,
  ty: Ty<'tcx>,
) -> Option<Ty<'tcx>> {
  let pointee = match ty.kind() {
    ty::Ref(_, pointee, Mutability::Not) => *pointee,
    ty::Adt(adt, args)
      if matches!(tcx.crate_name(adt.did().krate).as_str(), "alloc" | "std")
        && matches!(tcx.item_name(adt.did()).as_str(), "Rc" | "Arc" | "Weak") =>
    {
      args.type_at(0)
    }
    _ => return None,
  };
  let pointee = tcx.erase_and_anonymize_regions(pointee);
  (!pointee.is_freeze(tcx, typing_env)).then_some(pointee)
}

/// Whether writing `mutated` may write the shared `state`: it is inside the state,
/// or owns the handle directly. Rewriting a reference is not writing its pointee.
fn writes_state<'tcx>(state: Place<'tcx>, mutated: Place<'tcx>) -> bool {
  if state.local != mutated.local {
    return false;
  }
  if mutated.projection.starts_with(&state.projection) {
    return true;
  }
  state.projection.starts_with(&mutated.projection)
    && !state.projection[mutated.projection.len() ..].contains(&ProjectionElem::Deref)
}

pub(crate) struct SharedHandles<'tcx> {
  /// Normalized place standing for each handle's shared state, and its group.
  /// A reference stands for its pointee; an `Rc`/`Arc` stands for itself, as
  /// its pointee has no place of its own.
  handles: Vec<(Place<'tcx>, usize)>,
  groups: Vec<Vec<Place<'tcx>>>,
}

impl<'tcx> SharedHandles<'tcx> {
  /// Returns `None` unless at least two handles share a pointee type.
  pub fn build(place_info: &PlaceInfo<'_, 'tcx>) -> Option<Self> {
    let (tcx, body, def_id) = (place_info.tcx, place_info.body, place_info.def_id);
    let typing_env = tcx.typing_env_normalized_for_post_analysis(def_id);
    let locals = body
      .local_decls
      .indices()
      .flat_map(|local| Place::from_local(local, tcx).interior_places(tcx, body, def_id));
    let candidates = locals.chain(place_info.all_args().map(|(place, _)| place));

    let mut by_pointee: FxHashMap<Ty<'tcx>, Vec<Place<'tcx>>> = FxHashMap::default();
    for place in candidates {
      let ty = place.ty(&body.local_decls, tcx).ty;
      let Some(pointee) = interior_pointee(tcx, typing_env, ty) else {
        continue;
      };
      let state = if ty.is_ref() {
        tcx.mk_place_deref(place)
      } else {
        place
      };
      let members = by_pointee.entry(pointee).or_default();
      let state = place_info.normalize(state);
      if !members.contains(&state) {
        members.push(state);
      }
    }

    let groups = by_pointee
      .into_values()
      .filter(|members| members.len() > 1)
      .collect::<Vec<_>>();
    let handles = groups
      .iter()
      .enumerate()
      .flat_map(|(group, members)| members.iter().map(move |place| (*place, group)))
      .collect::<Vec<_>>();
    (!handles.is_empty()).then_some(SharedHandles { handles, groups })
  }

  /// Handles that may share state with a handle overlapping the normalized
  /// `mutated` place, excluding that handle itself.
  pub fn possibly_shared(
    &self,
    mutated: Place<'tcx>,
  ) -> impl Iterator<Item = Place<'tcx>> + '_ {
    self
      .handles
      .iter()
      .filter(move |(handle, _)| writes_state(*handle, mutated))
      .flat_map(move |(handle, group)| {
        self.groups[*group]
          .iter()
          .copied()
          .filter(move |other| other != handle)
      })
  }
}
