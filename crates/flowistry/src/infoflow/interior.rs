//! Interior mutability: state behind a shared reference that can be mutated anyway,
//! because it is (or contains) an `UnsafeCell`, e.g. a `Cell`, `RefCell`, `Mutex` or
//! atomic.
//!
//! A callee receiving a shared reference to such state may write it (e.g.
//! `Cell::set(&self, ..)`), which the modular approximation of `&mut` operands
//! does not account for. Whether a type has interior mutability is decided with
//! [`ErasedTy::is_freeze`]: a type that is not `Freeze` may be mutated through a
//! shared reference.

use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::{Mutability, Operand, Place},
  ty::{Instance, InstanceKind, TyCtxt, TyKind, TypingEnv},
};
use rustc_utils::PlaceExt;

use super::callsite::cmp_places_structurally;
use crate::mir::{placeinfo::PlaceInfo, utils::ErasedTy};

/// Whether a callee may mutate the interior-mutable state behind the shared
/// references it receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteriorMutation {
  /// It may, as any unknown function may.
  Possible,
  /// It is a standard-library function known to leave that state unchanged (see
  /// [`leaves_interior_state_unchanged`]).
  None,
}

impl InteriorMutation {
  /// The interior mutation of the function called by `func` from `caller`.
  pub fn of_call<'tcx>(tcx: TyCtxt<'tcx>, caller: DefId, func: &Operand<'tcx>) -> Self {
    if leaves_interior_state_unchanged(tcx, caller, func) {
      InteriorMutation::None
    } else {
      InteriorMutation::Possible
    }
  }
}

/// The kinds of shared handles to interior-mutable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handle<'tcx> {
  /// A shared reference `&T`: its state is its pointee, a place of the body.
  Ref { pointee: ErasedTy<'tcx> },
  /// An `Rc<T>`, `Arc<T>` or one of their `Weak`s: its pointee is not a place of
  /// the body, so the handle stands for it.
  Owning { pointee: ErasedTy<'tcx> },
}

impl<'tcx> Handle<'tcx> {
  /// The handle of type `ty`, if it is a shared handle to state that is not
  /// `Freeze`.
  pub fn parse(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    ty: ErasedTy<'tcx>,
  ) -> Option<Self> {
    let handle = match ty.ty().kind() {
      TyKind::Ref(_, pointee, Mutability::Not) => Handle::Ref {
        pointee: ErasedTy::new(tcx, *pointee),
      },
      TyKind::Adt(adt_def, args)
        if matches!(
          tcx.crate_name(adt_def.did().krate).as_str(),
          "alloc" | "std"
        ) && matches!(
          tcx.item_name(adt_def.did()).as_str(),
          "Rc" | "Arc" | "Weak"
        ) =>
      {
        Handle::Owning {
          pointee: ErasedTy::new(tcx, args.type_at(0)),
        }
      }
      _ => return None,
    };
    (!handle.pointee().is_freeze(tcx, typing_env)).then_some(handle)
  }

  /// The type of the shared state.
  pub fn pointee(self) -> ErasedTy<'tcx> {
    match self {
      Handle::Ref { pointee } | Handle::Owning { pointee } => pointee,
    }
  }
}

/// The places a callee may write through the interior mutability of the state that
/// `place` gives it shared access to: the innermost places that are not `Freeze`
/// among what is reachable from `place` through shared references (not `Freeze`
/// sibling fields, which stay independent), and the `Rc`/`Arc` handles to such state
/// (see [`Handle::Owning`]).
///
/// The places are ordered deterministically (see [`cmp_places_structurally`]).
pub(crate) fn interior_mutable_places<'tcx>(
  place_info: &PlaceInfo<'_, 'tcx>,
  place: Place<'tcx>,
) -> Vec<Place<'tcx>> {
  let tcx = place_info.tcx;
  let body = place_info.body;
  let typing_env = TypingEnv::post_analysis(tcx, place_info.def_id);
  let erased_ty =
    |place: Place<'tcx>| ErasedTy::new(tcx, place.ty(&body.local_decls, tcx).ty);
  let is_freeze = |place: Place<'tcx>| erased_ty(place).is_freeze(tcx, typing_env);
  let mutable = place_info.reachable_values(place, Mutability::Mut);
  let mut places = place_info
    .reachable_values(place, Mutability::Not)
    .iter()
    .filter(|shared| **shared != place && !mutable.contains(*shared))
    .flat_map(|shared| {
      if !is_freeze(*shared) {
        place_info
          .children(*shared)
          .into_iter()
          .filter(|child| place_info.children(*child).len() == 1 && !is_freeze(*child))
          .collect::<Vec<_>>()
      } else if matches!(
        Handle::parse(tcx, typing_env, erased_ty(*shared)),
        Some(Handle::Owning { .. })
      ) {
        vec![*shared]
      } else {
        Vec::new()
      }
    })
    .collect::<Vec<_>>();
  // A shared reference to interior-mutable state may view a place of another type
  // (e.g. `Cell::from_mut(&mut x)` views `x: i32` as a `Cell<i32>`): the whole place
  // may be written through it.
  for (_, pointers) in place.interior_pointers(tcx, body, place_info.def_id) {
    for (pointer, mutability) in pointers {
      let pointee = tcx.mk_place_deref(pointer);
      if mutability != Mutability::Not || is_freeze(pointee) {
        continue;
      }
      let pointee_ty = erased_ty(pointee);
      places.extend(
        place_info
          .aliases(pointee)
          .iter()
          .filter(|alias| **alias != pointee && erased_ty(**alias) != pointee_ty),
      );
    }
  }
  places.sort_by(|p1, p2| {
    cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
  });
  places.dedup();
  places
}

/// Whether `def_id` is defined in the standard library.
fn from_std(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
  matches!(
    tcx.crate_name(def_id.krate).as_str(),
    "core" | "alloc" | "std"
  )
}

/// Whether the function called by `func` from `caller` is a standard-library
/// function known to read, not write, the interior-mutable state it receives
/// shared references to (e.g. `Cell::get`, `RefCell::borrow`, `Mutex::lock`, atomic
/// `load`, or a comparison).
///
/// The decision is made on the implementation the call resolves to, not on the
/// method it names: a user's `impl Clone` may well mutate a `Cell`. Standard
/// implementations of comparison, hashing, formatting and cloning call those of
/// their type parameters (e.g. `Option<T>: Clone` calls `T::clone`), so they are
/// only exempt when no type of the call is defined outside the standard library.
/// Guard constructors (`borrow`, `lock`, ...) and the unwrapping of their results are
/// exempt: writes through the guards are connected to their owner by lifetimes.
pub(crate) fn leaves_interior_state_unchanged<'tcx>(
  tcx: TyCtxt<'tcx>,
  caller: DefId,
  func: &Operand<'tcx>,
) -> bool {
  let Some((def_id, args)) = func.const_fn_def() else {
    return false;
  };
  let typing_env = TypingEnv::post_analysis(tcx, caller);
  let args = tcx.erase_and_anonymize_regions(args);
  let Ok(Some(instance)) = Instance::try_resolve(tcx, typing_env, def_id, args) else {
    return false;
  };
  let InstanceKind::Item(resolved) = instance.def else {
    return false;
  };
  if !from_std(tcx, resolved) {
    return false;
  }
  let method = tcx.item_name(resolved);
  let method = method.as_str();

  let impl_id = tcx.impl_of_assoc(resolved);
  let self_adt = impl_id.and_then(|impl_id| {
    tcx
      .type_of(impl_id)
      .instantiate_identity()
      .skip_normalization()
      .ty_adt_def()
  });
  let self_name = self_adt.map(|adt| tcx.item_name(adt.did()));
  let self_name = self_name.as_ref().map(|name| name.as_str());

  // Whether no code outside the standard library can run: the types of the call
  // are all defined in the standard library.
  let only_std_types = instance.args.types().all(|ty| {
    ty.walk()
      .filter_map(|arg| arg.as_type())
      .all(|ty| match ty.kind() {
        TyKind::Adt(adt_def, _) => from_std(tcx, adt_def.did()),
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Array(..)
        | TyKind::Pat(..)
        | TyKind::Slice(_)
        | TyKind::RawPtr(..)
        | TyKind::Ref(..)
        | TyKind::Never
        | TyKind::Tuple(_) => true,
        _ => false,
      })
  });

  let trait_id = tcx
    .trait_of_assoc(resolved)
    .or_else(|| impl_id.and_then(|impl_id| tcx.impl_opt_trait_id(impl_id)));
  if let Some(trait_id) = trait_id {
    if !from_std(tcx, trait_id) {
      return false;
    }
    return match tcx.item_name(trait_id).as_str() {
      // Standard implementations only project a reference.
      "Deref" | "AsRef" | "Borrow" => true,
      // Cloning a shared handle clones the pointer, not the value.
      "Clone" if matches!(self_name, Some("Rc" | "Arc" | "Weak")) => true,
      "Clone" | "PartialEq" | "Eq" | "PartialOrd" | "Ord" | "Hash" | "Debug"
      | "Display" => only_std_types,
      _ => false,
    };
  }

  match self_name {
    Some("Cell" | "OnceCell" | "OnceLock") => method == "get",
    Some("RefCell") => matches!(
      method,
      "borrow" | "try_borrow" | "borrow_mut" | "try_borrow_mut"
    ),
    Some("Mutex") => matches!(method, "lock" | "try_lock" | "is_poisoned"),
    Some("RwLock") => matches!(
      method,
      "read" | "write" | "try_read" | "try_write" | "is_poisoned"
    ),
    Some("Rc" | "Arc") => matches!(method, "strong_count" | "weak_count" | "ptr_eq"),
    // E.g. the `LockResult` of `Mutex::lock`. The panic message of `unwrap` formats
    // the error, which could run user code.
    Some("Option" | "Result") => {
      only_std_types
        && matches!(
          method,
          "unwrap" | "expect" | "is_some" | "is_none" | "is_ok" | "is_err"
        )
    }
    Some(name) => name.starts_with("Atomic") && method == "load",
    None => false,
  }
}
