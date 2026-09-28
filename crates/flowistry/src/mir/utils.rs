//! A potpourri of utilities for working with the MIR, primarily exposed as extension traits.

use rustc_data_structures::fx::FxHashSet as HashSet;
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::*,
  ty::{GenericArgKind, RegionKind, RegionVid, Ty, TyCtxt, TypingEnv},
};
use rustc_span::Spanned;
use rustc_utils::{BodyExt, OperandExt, PlaceExt};

use crate::extensions::{EvalMode, MutabilityMode};

/// An unordered collections of MIR [`Place`]s.
///
/// *Note:* this used to be implemented as an [`IndexSet`](indexical::IndexSet),
/// but in practice it was very hard to determine up-front a fixed domain of
/// [`Place`]s that was not "every possible place in the body".
pub type PlaceSet<'tcx> = HashSet<Place<'tcx>>;

/// A type whose regions have all been erased (and anonymized).
///
/// Types of body places carry region variables from borrow checking. Trait-solving
/// queries such as [`Ty::is_freeze`] must not see those region variables (under
/// incremental compilation, they panic), and types that differ only in their regions
/// should compare equal. Such queries and comparisons are therefore only offered on
/// `ErasedTy`, whose only constructor erases regions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ErasedTy<'tcx>(Ty<'tcx>);

impl<'tcx> ErasedTy<'tcx> {
  /// Erases and anonymizes all regions of `ty`.
  pub fn new(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Self {
    ErasedTy(tcx.erase_and_anonymize_regions(ty))
  }

  /// The underlying (region-erased) type.
  pub fn ty(self) -> Ty<'tcx> {
    self.0
  }

  /// Whether this is the unit type `()`.
  pub fn is_unit(self) -> bool {
    self.0.is_unit()
  }

  /// Whether this type is `Freeze`, i.e. has no interior mutability.
  pub fn is_freeze(self, tcx: TyCtxt<'tcx>, typing_env: TypingEnv<'tcx>) -> bool {
    self.0.is_freeze(tcx, typing_env)
  }
}

/// Maximum number of projections of an argument's interior pointer for which the
/// analysis assumes the argument holds a loan.
///
/// Pointers nested more deeply in an argument are ignored, both when seeding the
/// initial loans of arguments in [`Aliases`](super::aliases::Aliases) and when
/// initializing the dependencies of arguments. This bounds the cost of functions with
/// many pointers in their inputs, and is not sound: see
/// [`PlaceInfo::arg_pointers_truncated`](super::placeinfo::PlaceInfo::arg_pointers_truncated).
pub(crate) const MAX_ARG_POINTER_DEPTH: usize = 2;

/// Given the arguments to a function, returns all projections of the arguments that are mutable pointers.
///
/// Reads the ambient [`MutabilityMode`] (see [`EvalMode::from_ambient`]).
pub fn arg_mut_ptrs<'tcx>(
  args: &[(usize, Place<'tcx>)],
  tcx: TyCtxt<'tcx>,
  body: &Body<'tcx>,
  def_id: DefId,
) -> Vec<(usize, Place<'tcx>)> {
  let ignore_mut = match EvalMode::from_ambient().mutability_mode {
    MutabilityMode::IgnoreMut => true,
    MutabilityMode::DistinguishMut => false,
  };
  args
    .iter()
    .flat_map(|(i, place)| {
      place
        .interior_pointers(tcx, body, def_id)
        .into_values()
        .flat_map(|places| {
          places
            .into_iter()
            .filter_map(|(place, mutability)| match mutability {
              Mutability::Mut => Some(place),
              Mutability::Not => ignore_mut.then_some(place),
            })
        })
        .map(move |place| (*i, tcx.mk_place_deref(place)))
    })
    .collect::<Vec<_>>()
}

/// Given the arguments to a function, returns all places in the arguments.
pub fn arg_places<'tcx>(args: &[Spanned<Operand<'tcx>>]) -> Vec<(usize, Place<'tcx>)> {
  args
    .iter()
    .enumerate()
    .filter_map(|(i, arg)| arg.node.as_place().map(move |place| (i, place)))
    .collect::<Vec<_>>()
}

/// A hack to temporary hack to reduce spurious dependencies in generators
/// arising from async functions.
///
/// The issue is that the `&mut std::task::Context` variable interferes with both
/// the modular approximation and the alias analysis. As a patch up, we ignore subset
/// constraints arising from lifetimes appearing in the Context type, as well as ignore
/// any place of type Context in function calls.
///
/// See test: async_two_await
pub(crate) struct AsyncHack<'a, 'tcx> {
  context_ty: Option<Ty<'tcx>>,
  tcx: TyCtxt<'tcx>,
  body: &'a Body<'tcx>,
}

impl<'a, 'tcx> AsyncHack<'a, 'tcx> {
  pub fn new(tcx: TyCtxt<'tcx>, body: &'a Body<'tcx>, def_id: DefId) -> Self {
    let context_ty = body.async_context(tcx, def_id);
    AsyncHack {
      context_ty,
      tcx,
      body,
    }
  }

  pub fn ignore_regions(&self) -> HashSet<RegionVid> {
    match self.context_ty {
      Some(context_ty) => context_ty
        .walk()
        .filter_map(|part| match part.kind() {
          GenericArgKind::Lifetime(r) => match r.kind() {
            RegionKind::ReVar(rv) => Some(rv),
            _ => None,
          },
          _ => None,
        })
        .collect::<HashSet<_>>(),
      None => HashSet::default(),
    }
  }

  pub fn ignore_place(&self, place: Place<'tcx>) -> bool {
    match self.context_ty {
      Some(context_ty) => {
        let place_ty = place.ty(&self.body.local_decls, self.tcx).ty;
        ErasedTy::new(self.tcx, place_ty) == ErasedTy::new(self.tcx, context_ty)
      }
      None => false,
    }
  }
}

#[cfg(test)]
mod test {
  use rustc_middle::ty::TypeVisitableExt;
  use rustc_utils::test_utils::Placer;

  use super::*;
  use crate::test_utils::{self, IncrementalDir};

  fn erased_place_ty<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    place: Place<'tcx>,
  ) -> ErasedTy<'tcx> {
    ErasedTy::new(tcx, place.ty(body.local_decls(), tcx).ty)
  }

  /// Trait queries on body types go through `ErasedTy`: the raw types carry region
  /// variables, which incremental compilation cannot hash (e.g. `is_freeze` on the raw
  /// type of `*x` panics with "region variables should not be hashed").
  #[test]
  fn erased_ty_queries_under_incremental() {
    let input = r#"
use std::cell::Cell;
fn f<'a>(x: &'a Cell<&'a i32>, y: &'a (i32, &'a i32)) {}
"#;
    let incremental = IncrementalDir::new();
    test_utils::compile_body_with_args(
      input,
      &incremental.args(),
      |tcx, body_id, body_with_facts| {
        let body = &body_with_facts.body;
        let def_id = tcx.hir_body_owner_def_id(body_id);
        let typing_env = TypingEnv::post_analysis(tcx, def_id);
        let p = Placer::new(tcx, body);
        let erased = |place| erased_place_ty(tcx, body, place);

        let x = p.local("x");
        let y = p.local("y");
        assert!(x.mk().ty(body.local_decls(), tcx).ty.has_infer_regions());
        assert!(!erased(x.mk()).ty().has_infer_regions());

        assert!(erased(x.mk()).is_freeze(tcx, typing_env));
        assert!(!erased(x.deref().mk()).is_freeze(tcx, typing_env));
        assert!(erased(y.deref().mk()).is_freeze(tcx, typing_env));
        assert!(!erased(y.mk()).is_unit());

        // Types that differ only in their regions are equal once erased.
        let ref_i32 = Ty::new_imm_ref(tcx, tcx.lifetimes.re_erased, tcx.types.i32);
        assert_eq!(erased(y.deref().field(1).mk()), ErasedTy::new(tcx, ref_i32));
      },
    );
  }
}
