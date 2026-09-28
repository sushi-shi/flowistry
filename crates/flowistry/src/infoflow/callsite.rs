//! The boundary between a caller and a callee analyzed in
//! [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse).
//!
//! The rows of a callee's flow analysis are places of the *callee* body. They are
//! parsed once, here, into body-independent [`EffectPath`]s, and then translated into
//! places of the *caller* body with [`CallSite::translate`], using only caller types.
//! Translation is total: when a path cannot be followed in the caller (e.g. through a
//! private field), it degrades to a coarser caller place ([`Target::Coarsened`]) and
//! says what was lost, instead of silently dropping the effect.

use std::cmp::Ordering;

use rustc_abi::{FieldIdx, VariantIdx};
use rustc_hir::def_id::DefId;
use rustc_middle::{
  mir::{
    Body, HasLocalDecls, Local, Operand, Place, PlaceElem, PlaceTy, ProjectionElem,
    RETURN_PLACE, TerminatorKind,
  },
  ty::{Ty, TyCtxt, TyKind},
};
use rustc_span::Spanned;
use rustc_utils::PlaceExt;
use smallvec::SmallVec;

use crate::mir::{placeinfo::NormPlace, utils::ErasedTy};

/// Why a call is analyzed with the modular approximation instead of recursing
/// into the callee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackReason {
  /// The called function is not a constant (e.g. a function pointer).
  FuncNotConstant,
  /// The called constant is not a function definition.
  NotFnDef,
  /// The callee never returns, so it has no exit state.
  ReturnsNever,
  /// The callee is not defined in the local crate.
  NotLocal,
  /// The callee has no body.
  NoBody,
  /// An argument contains an `FnMut` or `FnOnce` closure.
  FnMutOrOnceClosureArg,
  /// The callee is already being analyzed (recursion).
  RecursiveCall,
  /// The terminator is not a call.
  NotACall,
  /// The call operands do not match the callee's parameters.
  AbiMismatch,
  /// The callee resolves to a different body than the one named by the call.
  ResolvesElsewhere,
}

/// An operand of a call, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgOperand<'tcx> {
  /// A place of the caller body (moved or copied).
  Place(Place<'tcx>),
  /// A value with no caller place, e.g. a constant.
  Constant {
    /// The type of the value.
    ty: Ty<'tcx>,
  },
}

/// The operands of a call, parsed.
#[derive(Debug, Clone)]
pub(crate) struct CallOperands<'tcx> {
  pub args: Vec<ArgOperand<'tcx>>,
  pub destination: Place<'tcx>,
}

impl<'tcx> CallOperands<'tcx> {
  pub fn parse(
    args: &[Spanned<Operand<'tcx>>],
    destination: Place<'tcx>,
    body: &Body<'tcx>,
    tcx: TyCtxt<'tcx>,
  ) -> Self {
    let args = args
      .iter()
      .map(|arg| match &arg.node {
        Operand::Copy(place) | Operand::Move(place) => ArgOperand::Place(*place),
        op @ (Operand::Constant(_) | Operand::RuntimeChecks(_)) => ArgOperand::Constant {
          ty: op.ty(body.local_decls(), tcx),
        },
      })
      .collect();
    CallOperands { args, destination }
  }

  fn arg_ty(&self, i: usize, body: &Body<'tcx>, tcx: TyCtxt<'tcx>) -> Option<Ty<'tcx>> {
    Some(match self.args.get(i)? {
      ArgOperand::Place(place) => place.ty(body.local_decls(), tcx).ty,
      ArgOperand::Constant { ty } => *ty,
    })
  }
}

/// A parameter position of a callee, in terms of the caller's operands.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Debug)]
pub(crate) enum ArgPos {
  /// The parameter is the caller's operand at this index.
  Plain(usize),
  /// The parameter is a field of the tuple passed as the caller's operand `tuple`
  /// (the "rust-call" ABI of closure bodies).
  Tupled { tuple: usize, field: FieldIdx },
}

impl ArgPos {
  /// The index of the caller operand holding this parameter.
  pub fn operand(self) -> usize {
    match self {
      ArgPos::Plain(i) | ArgPos::Tupled { tuple: i, .. } => i,
    }
  }
}

/// The root of an [`EffectPath`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Debug)]
pub(crate) enum EffectRoot {
  /// The callee's return place, i.e. the call destination.
  Return,
  /// A parameter of the callee.
  Arg(ArgPos),
}

/// A projection step of an [`EffectPath`]. Unlike [`PlaceElem`], it carries no types,
/// so it means the same thing in the caller and the callee.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Debug)]
pub(crate) enum PathElem {
  Deref,
  Field(FieldIdx),
  Downcast(VariantIdx),
  /// Any element of an array or slice.
  AnyIndex,
}

/// Whether an [`EffectPath`] covers the whole callee place.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Debug)]
pub(crate) enum PathTail {
  /// The path is the whole callee place.
  Complete,
  /// The callee place continued with a projection that has no body-independent
  /// meaning (`OpaqueCast`, `UnwrapUnsafeBinder`), so the path stops before it.
  Truncated {
    /// Whether the dropped part of the place went through a pointer.
    dropped_deref: bool,
  },
}

/// A body-independent description of a callee place reachable by the caller.
#[derive(Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Debug)]
pub(crate) struct EffectPath {
  pub root: EffectRoot,
  pub elems: SmallVec<[PathElem; 4]>,
  pub tail: PathTail,
}

impl EffectPath {
  /// Parses the projection of a callee place.
  fn parse(root: EffectRoot, projection: &[PlaceElem<'_>]) -> Self {
    let mut elems = SmallVec::new();
    let mut tail = PathTail::Complete;
    for (i, elem) in projection.iter().enumerate() {
      let path_elem = match elem {
        ProjectionElem::Deref => PathElem::Deref,
        ProjectionElem::Field(field, _) => PathElem::Field(*field),
        ProjectionElem::Downcast(_, variant) => PathElem::Downcast(*variant),
        ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {
          PathElem::AnyIndex
        }
        // A subslice is the same place as the whole slice (as in normalization).
        ProjectionElem::Subslice { .. } => continue,
        ProjectionElem::OpaqueCast(_) | ProjectionElem::UnwrapUnsafeBinder(_) => {
          let dropped_deref = projection[i ..]
            .iter()
            .any(|elem| matches!(elem, ProjectionElem::Deref));
          tail = PathTail::Truncated { dropped_deref };
          break;
        }
      };
      elems.push(path_elem);
    }
    EffectPath { root, elems, tail }
  }
}

/// How the callee's parameters correspond to the caller's operands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CalleeAbi {
  /// Parameter `_k` is operand `k - 1`.
  Direct { arg_count: usize },
  /// A closure body called through `Fn*::call*(closure, (a1, .., an))`: parameter
  /// `_1` is the closure (operand 0), parameters `_2 ..` are the `untupled` fields of
  /// the tuple passed as operand 1.
  ClosureBody { untupled: usize },
}

impl CalleeAbi {
  /// Determines the ABI of `callee` from its body and the call operands.
  pub fn parse<'tcx>(
    tcx: TyCtxt<'tcx>,
    callee: DefId,
    callee_body: &Body<'tcx>,
    caller_body: &Body<'tcx>,
    ops: &CallOperands<'tcx>,
  ) -> Result<Self, FallbackReason> {
    let arg_count = callee_body.arg_count;
    if tcx.is_closure_like(callee) && callee_body.spread_arg.is_none() {
      if ops.args.len() != 2 {
        return Err(FallbackReason::AbiMismatch);
      }
      let tuple_ty = ops
        .arg_ty(1, caller_body, tcx)
        .ok_or(FallbackReason::AbiMismatch)?;
      let TyKind::Tuple(fields) = tuple_ty.kind() else {
        return Err(FallbackReason::AbiMismatch);
      };
      if arg_count != 1 + fields.len() {
        return Err(FallbackReason::AbiMismatch);
      }
      Ok(CalleeAbi::ClosureBody {
        untupled: fields.len(),
      })
    } else {
      // A C-variadic callee receives its variadic operands through a trailing `VaList`
      // parameter. As before, parameters map positionally to the leading operands
      // (the `VaList` to the first variadic operand, if any), and the remaining
      // operands are ignored.
      let c_variadic = tcx.fn_sig(callee).skip_binder().skip_binder().c_variadic();
      let fits = if c_variadic {
        ops.args.len() + 1 >= arg_count
      } else {
        ops.args.len() == arg_count
      };
      if !fits {
        return Err(FallbackReason::AbiMismatch);
      }
      Ok(CalleeAbi::Direct { arg_count })
    }
  }

  /// The position of a callee local among the caller's operands, if it is a parameter.
  pub fn arg_pos(&self, callee_local: Local) -> Option<ArgPos> {
    let k = callee_local.as_usize();
    match *self {
      CalleeAbi::Direct { arg_count } => {
        (1 ..= arg_count).contains(&k).then(|| ArgPos::Plain(k - 1))
      }
      CalleeAbi::ClosureBody { untupled } => match k {
        1 => Some(ArgPos::Plain(0)),
        _ if (2 .. 2 + untupled).contains(&k) => Some(ArgPos::Tupled {
          tuple: 1,
          field: FieldIdx::from_usize(k - 2),
        }),
        _ => None,
      },
    }
  }
}

/// A row of a callee's flow analysis, i.e. a (normalized) place of the callee body.
///
/// This is a distinct type from the caller's [`NormPlace`] rows so that a callee row
/// cannot be looked up in, or written to, a caller's state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct CalleeRow<'tcx>(NormPlace<'tcx>);

impl<'tcx> CalleeRow<'tcx> {
  /// Declares a row of a callee's state to be a callee row.
  pub fn new(row: NormPlace<'tcx>) -> Self {
    CalleeRow(row)
  }

  /// The row as a key of the callee's state.
  pub fn key(self) -> NormPlace<'tcx> {
    self.0
  }

  /// The (region-erased) type of the row in the callee body.
  pub fn ty(self, callee_body: &Body<'tcx>, tcx: TyCtxt<'tcx>) -> ErasedTy<'tcx> {
    self.0.ty(callee_body, tcx)
  }

  /// A total order on rows that depends only on their structure, not on interning
  /// addresses (see [`cmp_places_structurally`]).
  pub fn cmp_structural(self, other: Self) -> Ordering {
    cmp_places_structurally(
      self.0.local(),
      self.0.projection(),
      other.0.local(),
      other.0.projection(),
    )
  }
}

/// The role of a callee row for the caller.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum RowRole {
  /// (A part of) the callee's return place.
  Return(EffectPath),
  /// (A part of) a parameter itself, not behind a pointer: private to the callee.
  ArgDirect(EffectPath),
  /// A place behind a pointer passed as a parameter: shared with the caller.
  ArgPointee(EffectPath),
  /// A local of the callee that the caller cannot observe.
  Internal,
}

/// What a coarsened [`Target`] lost.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Coarsening {
  /// The effect is somewhere inside the target place, not behind a pointer.
  Interior,
  /// The effect may be behind a pointer stored inside the target place.
  ThroughPointer,
}

/// A caller place that an effect translates to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Target<'tcx> {
  /// The caller place of the effect.
  Exact(Place<'tcx>),
  /// The longest prefix of the effect's path that the caller can name.
  Coarsened {
    place: Place<'tcx>,
    lost: Coarsening,
  },
}

impl<'tcx> Target<'tcx> {
  /// The caller place, exact or coarsened.
  pub fn place(self) -> Place<'tcx> {
    match self {
      Target::Exact(place) | Target::Coarsened { place, .. } => place,
    }
  }
}

/// The result of translating an [`EffectPath`] into the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Resolved<'tcx> {
  Target(Target<'tcx>),
  /// The path starts at an operand with no caller place (e.g. a constant), so the
  /// effect has no caller provenance.
  NoCallerState,
}

/// A call from a caller body to a callee body, with the call operands parsed.
pub(crate) struct CallSite<'a, 'tcx> {
  tcx: TyCtxt<'tcx>,
  caller_def_id: DefId,
  caller_body: &'a Body<'tcx>,
  ops: CallOperands<'tcx>,
  abi: CalleeAbi,
}

impl<'a, 'tcx> CallSite<'a, 'tcx> {
  pub fn parse(
    tcx: TyCtxt<'tcx>,
    caller_def_id: DefId,
    caller_body: &'a Body<'tcx>,
    call: &TerminatorKind<'tcx>,
    callee: DefId,
    callee_body: &Body<'tcx>,
  ) -> Result<Self, FallbackReason> {
    let TerminatorKind::Call {
      args, destination, ..
    } = call
    else {
      return Err(FallbackReason::NotACall);
    };
    let ops = CallOperands::parse(args, *destination, caller_body, tcx);
    let abi = CalleeAbi::parse(tcx, callee, callee_body, caller_body, &ops)?;
    Ok(CallSite {
      tcx,
      caller_def_id,
      caller_body,
      ops,
      abi,
    })
  }

  /// The destination of the call.
  pub fn destination(&self) -> Place<'tcx> {
    self.ops.destination
  }

  #[cfg(test)]
  pub fn abi(&self) -> CalleeAbi {
    self.abi
  }

  #[cfg(test)]
  pub fn operands(&self) -> &CallOperands<'tcx> {
    &self.ops
  }

  /// Classifies a row of the callee's state.
  pub fn classify(&self, row: CalleeRow<'tcx>) -> RowRole {
    let place = row.key();
    let root = if place.local() == RETURN_PLACE {
      EffectRoot::Return
    } else {
      match self.abi.arg_pos(place.local()) {
        Some(pos) => EffectRoot::Arg(pos),
        None => return RowRole::Internal,
      }
    };
    let is_indirect = place
      .projection()
      .iter()
      .any(|elem| matches!(elem, ProjectionElem::Deref));
    let path = EffectPath::parse(root, place.projection());
    match root {
      EffectRoot::Return => RowRole::Return(path),
      EffectRoot::Arg(_) if is_indirect => RowRole::ArgPointee(path),
      EffectRoot::Arg(_) => RowRole::ArgDirect(path),
    }
  }

  /// The caller operands passed to callee parameters whose type is opaque to the
  /// callee (a type parameter, an alias or a trait object, possibly nested): the
  /// callee's analysis cannot see pointers hidden in them.
  pub fn opaque_operands(&self, callee_body: &Body<'tcx>) -> SmallVec<[usize; 4]> {
    let mut operands = callee_body
      .args_iter()
      .filter(|local| {
        callee_body.local_decls[*local].ty.walk().any(|arg| {
          arg.as_type().is_some_and(|ty| {
            matches!(
              ty.kind(),
              TyKind::Param(_) | TyKind::Alias(..) | TyKind::Dynamic(..)
            )
          })
        })
      })
      .filter_map(|local| self.abi.arg_pos(local).map(ArgPos::operand))
      .collect::<SmallVec<[usize; 4]>>();
    operands.dedup();
    operands
  }

  /// Translates an effect path into a place of the caller.
  ///
  /// Total: every path translates, possibly to a coarser place, and never panics.
  pub fn translate(&self, path: &EffectPath) -> Resolved<'tcx> {
    let tcx = self.tcx;
    let root = match path.root {
      EffectRoot::Return => self.ops.destination,
      EffectRoot::Arg(pos) => {
        let Some(ArgOperand::Place(operand)) = self.ops.args.get(pos.operand()) else {
          return Resolved::NoCallerState;
        };
        match pos {
          ArgPos::Plain(_) => *operand,
          ArgPos::Tupled { field, .. } => {
            let ty = operand.ty(self.caller_body.local_decls(), tcx).ty;
            match ty.kind() {
              TyKind::Tuple(fields) if field.as_usize() < fields.len() => {
                tcx.mk_place_field(*operand, field, fields[field.as_usize()])
              }
              _ => {
                return Resolved::Target(Target::Coarsened {
                  place: *operand,
                  lost: coarsening(path, 0),
                });
              }
            }
          }
        }
      }
    };

    let mut projection = root.projection.to_vec();
    let mut place_ty = root.ty(self.caller_body.local_decls(), tcx);
    for (i, elem) in path.elems.iter().enumerate() {
      match self.step(place_ty, *elem) {
        Some((place_elem, next_ty)) => {
          projection.push(place_elem);
          place_ty = next_ty;
        }
        None => {
          return Resolved::Target(Target::Coarsened {
            place: Place::make(root.local, &projection, tcx),
            lost: coarsening(path, i),
          });
        }
      }
    }

    let place = Place::make(root.local, &projection, tcx);
    Resolved::Target(match path.tail {
      PathTail::Complete => Target::Exact(place),
      PathTail::Truncated { .. } => Target::Coarsened {
        place,
        lost: coarsening(path, path.elems.len()),
      },
    })
  }

  /// Follows one path element from a caller place of type `place_ty`, if the caller
  /// can name the result.
  fn step(
    &self,
    place_ty: PlaceTy<'tcx>,
    elem: PathElem,
  ) -> Option<(PlaceElem<'tcx>, PlaceTy<'tcx>)> {
    let tcx = self.tcx;
    let ty = place_ty.ty;
    match elem {
      PathElem::Deref => {
        if place_ty.variant_index.is_some() {
          return None;
        }
        let pointee = ty.builtin_deref(true)?;
        Some((ProjectionElem::Deref, PlaceTy::from_ty(pointee)))
      }
      PathElem::Field(field) => {
        let field_ty = match ty.kind() {
          TyKind::Adt(adt_def, args) => {
            let variant = match place_ty.variant_index {
              Some(variant) => adt_def.variants().get(variant)?,
              None if adt_def.is_enum() => return None,
              None => adt_def.non_enum_variant(),
            };
            let field_def = variant.fields.get(field)?;
            if !field_def.vis.is_accessible_from(self.caller_def_id, tcx) {
              return None;
            }
            field_def.ty(tcx, args)
          }
          _ if place_ty.variant_index.is_some() => return None,
          TyKind::Tuple(fields) => *fields.get(field.as_usize())?,
          TyKind::Closure(_, args) => {
            *args.as_closure().upvar_tys().get(field.as_usize())?
          }
          TyKind::CoroutineClosure(_, args) => *args
            .as_coroutine_closure()
            .upvar_tys()
            .get(field.as_usize())?,
          TyKind::Coroutine(_, args) => {
            *args.as_coroutine().upvar_tys().get(field.as_usize())?
          }
          _ => return None,
        };
        Some((
          ProjectionElem::Field(field, field_ty),
          PlaceTy::from_ty(field_ty),
        ))
      }
      PathElem::Downcast(variant) => {
        let TyKind::Adt(adt_def, _) = ty.kind() else {
          return None;
        };
        if !adt_def.is_enum() || place_ty.variant_index.is_some() {
          return None;
        }
        let variant_def = adt_def.variants().get(variant)?;
        Some((
          ProjectionElem::Downcast(Some(variant_def.name), variant),
          PlaceTy {
            ty,
            variant_index: Some(variant),
          },
        ))
      }
      PathElem::AnyIndex => {
        if place_ty.variant_index.is_some() {
          return None;
        }
        let elem_ty = ty.builtin_index()?;
        // `[_0]` is the canonical index of normalized places.
        Some((
          ProjectionElem::Index(Local::from_u32(0)),
          PlaceTy::from_ty(elem_ty),
        ))
      }
    }
  }
}

/// A total order on places that depends only on their structure: the local, then the
/// projection elements (kind, then indices). Types in projections are ignored.
///
/// Unlike orders derived from hashing interned places, it is the same in every run.
pub(crate) fn cmp_places_structurally(
  local1: Local,
  projection1: &[PlaceElem<'_>],
  local2: Local,
  projection2: &[PlaceElem<'_>],
) -> Ordering {
  fn key(elem: &PlaceElem<'_>) -> (u8, u64, u64, bool) {
    match *elem {
      ProjectionElem::Deref => (0, 0, 0, false),
      ProjectionElem::Field(field, _) => (1, field.as_u32().into(), 0, false),
      ProjectionElem::Index(local) => (2, local.as_u32().into(), 0, false),
      ProjectionElem::ConstantIndex {
        offset,
        min_length,
        from_end,
      } => (3, offset, min_length, from_end),
      ProjectionElem::Subslice { from, to, from_end } => (4, from, to, from_end),
      ProjectionElem::Downcast(_, variant) => (5, variant.as_u32().into(), 0, false),
      ProjectionElem::OpaqueCast(_) => (6, 0, 0, false),
      ProjectionElem::UnwrapUnsafeBinder(_) => (7, 0, 0, false),
    }
  }
  local1
    .cmp(&local2)
    .then_with(|| projection1.iter().map(key).cmp(projection2.iter().map(key)))
}

/// What is lost by stopping the translation of `path` before its element `i`.
fn coarsening(path: &EffectPath, i: usize) -> Coarsening {
  let dropped_deref = path.elems[i ..].contains(&PathElem::Deref)
    || matches!(path.tail, PathTail::Truncated {
      dropped_deref: true
    });
  if dropped_deref {
    Coarsening::ThroughPointer
  } else {
    Coarsening::Interior
  }
}

#[cfg(test)]
mod test {
  use rustc_borrowck::consumers::BodyWithBorrowckFacts;
  use rustc_middle::{mir::BasicBlock, ty::GenericArgsRef};
  use rustc_utils::mir::borrowck_facts::get_body_with_borrowck_facts;
  use smallvec::smallvec;

  use super::*;
  use crate::{
    extensions::EvalMode,
    mir::placeinfo::PlaceInfo,
    test_utils::{self, IncrementalDir},
  };

  /// The calls of a body, in block order, with the called `FnDef` and its args.
  fn calls<'a, 'tcx>(
    body: &'a Body<'tcx>,
  ) -> Vec<(
    BasicBlock,
    &'a TerminatorKind<'tcx>,
    DefId,
    GenericArgsRef<'tcx>,
  )> {
    body
      .basic_blocks
      .iter_enumerated()
      .filter_map(|(bb, data)| {
        let kind = &data.terminator().kind;
        let TerminatorKind::Call { func, .. } = kind else {
          return None;
        };
        let (def_id, args) = func.const_fn_def()?;
        Some((bb, kind, def_id, args))
      })
      .collect()
  }

  fn path(root: EffectRoot, elems: &[PathElem]) -> EffectPath {
    EffectPath {
      root,
      elems: elems.iter().copied().collect(),
      tail: PathTail::Complete,
    }
  }

  fn target<'tcx>(resolved: Resolved<'tcx>) -> Target<'tcx> {
    match resolved {
      Resolved::Target(target) => target,
      Resolved::NoCallerState => panic!("no caller state"),
    }
  }

  fn exact<'tcx>(resolved: Resolved<'tcx>) -> Place<'tcx> {
    match target(resolved) {
      Target::Exact(place) => place,
      target => panic!("not exact: {target:?}"),
    }
  }

  fn tupled(field: usize) -> EffectRoot {
    EffectRoot::Arg(ArgPos::Tupled {
      tuple: 1,
      field: FieldIdx::from_usize(field),
    })
  }

  const CLOSURES: &str = r#"
fn caller() {
  let c = 1;
  let f0 = move || c;
  let f1 = |p: &mut i32| { *p = c; };
  let mut n = 0;
  let mut f2 = |p: &mut i32, q: i32| { *p = q; n += 1; };
  let s = String::new();
  let f3 = move |a: i32, b: &mut i32| { drop(s); *b = a; };
  let mut x = 0;
  f0();
  f1(&mut x);
  f2(&mut x, 2);
  f3(1, &mut x);
}
"#;

  #[test]
  fn closure_abi() {
    test_utils::compile_crate(CLOSURES, &[], check_closure_abi);
  }

  #[test]
  fn closure_abi_incremental() {
    let incremental = IncrementalDir::new();
    test_utils::compile_crate(CLOSURES, &incremental.args(), check_closure_abi);
  }

  fn check_closure_abi<'tcx>(tcx: TyCtxt<'tcx>) {
    let (caller_id, caller) = test_utils::body_named(tcx, "caller");
    let caller_body = &caller.body;
    let calls = calls(caller_body);
    assert_eq!(calls.len(), 5, "{calls:?}"); // String::new + four closure calls
    let closure_calls = calls
      .iter()
      .filter(|(_, _, _, args)| {
        args
          .types()
          .next()
          .is_some_and(|ty| matches!(ty.kind(), TyKind::Closure(..)))
      })
      .collect::<Vec<_>>();
    assert_eq!(closure_calls.len(), 4);

    let untupled = [0, 1, 2, 2];
    for ((_, kind, _, args), untupled) in closure_calls.into_iter().zip(untupled) {
      let TyKind::Closure(closure, _) = args.type_at(0).kind() else {
        panic!("not a closure call: {args:?}");
      };
      let closure = *closure;
      let callee = get_body_with_borrowck_facts(tcx, closure.expect_local());
      let callee_body = &callee.body;
      let site = CallSite::parse(
        tcx,
        caller_id.to_def_id(),
        caller_body,
        kind,
        closure,
        callee_body,
      )
      .unwrap();
      assert_eq!(site.abi(), CalleeAbi::ClosureBody { untupled });
      assert_eq!(
        site.abi().arg_pos(Local::from_usize(1)),
        Some(ArgPos::Plain(0))
      );
      assert_eq!(site.abi().arg_pos(Local::from_usize(2 + untupled)), None);
      for k in 0 .. untupled {
        assert_eq!(
          site.abi().arg_pos(Local::from_usize(2 + k)),
          Some(ArgPos::Tupled {
            tuple: 1,
            field: FieldIdx::from_usize(k)
          })
        );
      }

      let caller_ty = |place: Place<'tcx>| place.ty(caller_body.local_decls(), tcx).ty;
      let i32_ty = tcx.types.i32;
      match untupled {
        0 => {
          // The closure itself, by reference (Fn::call).
          let closure_ref = exact(
            site.translate(&path(EffectRoot::Arg(ArgPos::Plain(0)), &[PathElem::Deref])),
          );
          assert!(matches!(caller_ty(closure_ref).kind(), TyKind::Closure(..)));
        }
        1 => {
          // `*p` is `*(tuple.0)` in the caller.
          let pointee = exact(site.translate(&path(tupled(0), &[PathElem::Deref])));
          assert_eq!(caller_ty(pointee), i32_ty);
          let [tuple_field, ProjectionElem::Deref] = pointee.projection[..] else {
            panic!("{pointee:?}");
          };
          assert!(
            matches!(tuple_field, ProjectionElem::Field(f, _) if f.as_usize() == 0)
          );

          // Regression: the old arithmetic mapping sent `_2` to operand 1, i.e. the
          // tuple, and a Deref on it. That path now degrades instead of panicking.
          let old =
            site.translate(&path(EffectRoot::Arg(ArgPos::Plain(1)), &[PathElem::Deref]));
          assert!(matches!(target(old), Target::Coarsened {
            lost: Coarsening::ThroughPointer,
            ..
          }));

          // Rows of the closure body are classified by the ABI.
          let place_info =
            PlaceInfo::build_with_mode(tcx, closure, callee, EvalMode::default());
          let p = tcx.mk_place_deref(Place::from_local(Local::from_usize(2), tcx));
          let row = CalleeRow::new(place_info.normalize(p));
          assert_eq!(
            site.classify(row),
            RowRole::ArgPointee(path(tupled(0), &[PathElem::Deref]))
          );
          let local = CalleeRow::new(
            place_info.normalize(Place::from_local(Local::from_usize(2), tcx)),
          );
          assert_eq!(
            site.classify(local),
            RowRole::ArgDirect(path(tupled(0), &[]))
          );
        }
        _ => {
          let second = exact(site.translate(&path(tupled(1), &[])));
          let first = exact(site.translate(&path(tupled(0), &[])));
          if calls_by_value(&site) {
            // f3(a: i32, b: &mut i32), FnOnce: the closure is passed by value.
            assert_eq!(caller_ty(first), i32_ty);
            let b = exact(site.translate(&path(tupled(1), &[PathElem::Deref])));
            assert_eq!(caller_ty(b), i32_ty);
            assert!(matches!(caller_ty(second).kind(), TyKind::Ref(..)));
          } else {
            // f2(p: &mut i32, q: i32), FnMut.
            assert_eq!(caller_ty(second), i32_ty);
            let p = exact(site.translate(&path(tupled(0), &[PathElem::Deref])));
            assert_eq!(caller_ty(p), i32_ty);
            assert!(matches!(caller_ty(first).kind(), TyKind::Ref(..)));
          }
        }
      }

      // The call destination.
      let TerminatorKind::Call { destination, .. } = kind else {
        unreachable!()
      };
      assert_eq!(
        exact(site.translate(&path(EffectRoot::Return, &[]))),
        *destination
      );
    }
  }

  /// Whether the closure is passed by value (FnOnce::call_once).
  fn calls_by_value(site: &CallSite<'_, '_>) -> bool {
    let ArgOperand::Place(closure) = site.operands().args[0] else {
      return false;
    };
    !matches!(
      closure
        .ty(site.caller_body.local_decls(), site.tcx)
        .ty
        .kind(),
      TyKind::Ref(..)
    )
  }

  const C_VARIADIC: &str = r#"
#![feature(c_variadic)]
unsafe extern "C" fn v(x: i32, mut args: ...) -> i32 { x }
fn caller(a: i32) {
  unsafe { v(a, 2, 3); v(a); }
}
"#;

  #[test]
  fn c_variadic_callee() {
    test_utils::compile_crate(C_VARIADIC, &[], check_c_variadic);
  }

  fn check_c_variadic<'tcx>(tcx: TyCtxt<'tcx>) {
    let (caller_id, caller) = test_utils::body_named(tcx, "caller");
    let caller_body = &caller.body;
    let (v_id, v) = test_utils::body_named(tcx, "v");
    let calls = calls(caller_body);
    assert_eq!(calls.len(), 2, "{calls:?}");
    for (_, call, _, _) in calls {
      // The variadic operands are not parameters of the body: they are ignored.
      let site = CallSite::parse(
        tcx,
        caller_id.to_def_id(),
        caller_body,
        call,
        v_id.to_def_id(),
        &v.body,
      )
      .unwrap();
      assert_eq!(site.abi(), CalleeAbi::Direct {
        arg_count: v.body.arg_count
      });
      let x = exact(site.translate(&path(EffectRoot::Arg(ArgPos::Plain(0)), &[])));
      assert_eq!(x.ty(caller_body.local_decls(), tcx).ty, tcx.types.i32);
    }
  }

  #[test]
  fn structural_place_order() {
    test_utils::compile_crate("fn f() {}", &[], check_structural_order);
  }

  fn check_structural_order<'tcx>(tcx: TyCtxt<'tcx>) {
    let field = |i, ty| ProjectionElem::Field(FieldIdx::from_usize(i), ty);
    let (i32_ty, u8_ty) = (tcx.types.i32, tcx.types.u8);
    let cmp = |l1: usize, p1: &[PlaceElem<'tcx>], l2: usize, p2: &[PlaceElem<'tcx>]| {
      cmp_places_structurally(Local::from_usize(l1), p1, Local::from_usize(l2), p2)
    };
    assert_eq!(cmp(1, &[], 2, &[]), Ordering::Less);
    assert_eq!(cmp(2, &[], 1, &[field(0, i32_ty)]), Ordering::Greater);
    // Prefixes first.
    assert_eq!(cmp(1, &[], 1, &[ProjectionElem::Deref]), Ordering::Less);
    assert_eq!(
      cmp(1, &[field(1, i32_ty)], 1, &[
        field(0, i32_ty),
        field(0, i32_ty)
      ]),
      Ordering::Greater
    );
    // Field types do not matter.
    assert_eq!(
      cmp(1, &[field(0, i32_ty)], 1, &[field(0, u8_ty)]),
      Ordering::Equal
    );
    assert_eq!(
      cmp(1, &[ProjectionElem::Deref], 1, &[field(0, i32_ty)]),
      Ordering::Less
    );
  }

  const OPERANDS: &str = r#"
mod m {
  pub struct P { a: i32, pub b: i32 }
  pub struct W<'a> { p: &'a mut i32, pub q: &'a mut i32 }
}
trait Tr { fn t(&mut self); }
fn g<T>(t: T) {}
fn d(x: &mut dyn Tr) {}
fn h(p: &mut m::P, w: &mut m::W<'_>, k: i32) -> (i32, i32) { (0, 0) }
fn caller(x: &mut dyn Tr, p: &mut m::P, w: &mut m::W<'_>, y: &mut i32) {
  g(&mut *y);
  d(x);
  let r = h(p, w, 5);
}
"#;

  #[test]
  fn operands() {
    test_utils::compile_crate(OPERANDS, &[], check_operands);
  }

  fn check_operands<'tcx>(tcx: TyCtxt<'tcx>) {
    let (caller_id, caller) = test_utils::body_named(tcx, "caller");
    let caller_body = &caller.body;
    let caller_ty = |place: Place<'tcx>| place.ty(caller_body.local_decls(), tcx).ty;
    let calls = calls(caller_body);
    let [(_, g_call, g, _), (_, d_call, d, _), (_, h_call, h, _)] = calls[..] else {
      panic!("{calls:?}");
    };
    let callee_body = |def_id: DefId| -> &BodyWithBorrowckFacts<'_> {
      get_body_with_borrowck_facts(tcx, def_id.expect_local())
    };
    let site = |call, def_id| {
      CallSite::parse(
        tcx,
        caller_id.to_def_id(),
        caller_body,
        call,
        def_id,
        &callee_body(def_id).body,
      )
    };
    let plain = |i| EffectRoot::Arg(ArgPos::Plain(i));

    // Generic parameters are opaque to the callee.
    let g_site = site(g_call, g).unwrap();
    assert_eq!(g_site.abi(), CalleeAbi::Direct { arg_count: 1 });
    assert_eq!(&g_site.opaque_operands(&callee_body(g).body)[..], &[0]);

    // A trait object can be dereferenced, but has no fields.
    let d_site = site(d_call, d).unwrap();
    assert_eq!(&d_site.opaque_operands(&callee_body(d).body)[..], &[0]);
    let pointee = exact(d_site.translate(&path(plain(0), &[PathElem::Deref])));
    assert!(matches!(caller_ty(pointee).kind(), TyKind::Dynamic(..)));
    assert_eq!(
      target(d_site.translate(&path(plain(0), &[
        PathElem::Deref,
        PathElem::Field(FieldIdx::from_usize(0))
      ]))),
      Target::Coarsened {
        place: pointee,
        lost: Coarsening::Interior
      }
    );

    let h_site = site(h_call, h).unwrap();
    assert!(h_site.opaque_operands(&callee_body(h).body).is_empty());
    let field = |i| PathElem::Field(FieldIdx::from_usize(i));
    // Private field: coarsened to the struct.
    let p = exact(h_site.translate(&path(plain(0), &[PathElem::Deref])));
    assert_eq!(
      target(h_site.translate(&path(plain(0), &[PathElem::Deref, field(0)]))),
      Target::Coarsened {
        place: p,
        lost: Coarsening::Interior
      }
    );
    let b = exact(h_site.translate(&path(plain(0), &[PathElem::Deref, field(1)])));
    assert_eq!(caller_ty(b), tcx.types.i32);
    // Private pointer field, then a deref: the effect may be behind the pointer.
    let w = exact(h_site.translate(&path(plain(1), &[PathElem::Deref])));
    assert_eq!(
      target(h_site.translate(&path(plain(1), &[
        PathElem::Deref,
        field(0),
        PathElem::Deref
      ]))),
      Target::Coarsened {
        place: w,
        lost: Coarsening::ThroughPointer
      }
    );
    let q = exact(h_site.translate(&path(plain(1), &[
      PathElem::Deref,
      field(1),
      PathElem::Deref,
    ])));
    assert_eq!(caller_ty(q), tcx.types.i32);
    // The field type comes from the caller, with the caller's regions.
    let q_ptr = Place::make(q.local, &q.projection[.. q.projection.len() - 1], tcx);
    assert!(
      matches!(caller_ty(q_ptr).kind(), TyKind::Ref(region, _, _) if region.is_var())
    );
    // A constant operand has no caller state.
    assert_eq!(
      h_site.translate(&path(plain(2), &[])),
      Resolved::NoCallerState
    );
    // Return fields.
    let r1 = exact(h_site.translate(&path(EffectRoot::Return, &[field(1)])));
    assert_eq!(caller_ty(r1), tcx.types.i32);
    // A truncated path is coarsened.
    let truncated = EffectPath {
      tail: PathTail::Truncated {
        dropped_deref: false,
      },
      ..path(EffectRoot::Return, &[field(1)])
    };
    assert_eq!(target(h_site.translate(&truncated)), Target::Coarsened {
      place: r1,
      lost: Coarsening::Interior
    });

    // The operands of `d` do not fit the parameters of `h`.
    assert_eq!(site(d_call, h).err(), Some(FallbackReason::AbiMismatch));
    // Not a call.
    assert_eq!(
      site(&TerminatorKind::Return, h).err(),
      Some(FallbackReason::NotACall)
    );

    // Parsing a callee place into a path.
    let parsed = EffectPath::parse(plain(0), &[
      ProjectionElem::Deref,
      ProjectionElem::Index(Local::from_usize(3)),
      ProjectionElem::Subslice {
        from: 0,
        to: 1,
        from_end: true,
      },
    ]);
    assert_eq!(parsed, EffectPath {
      root: plain(0),
      elems: smallvec![PathElem::Deref, PathElem::AnyIndex],
      tail: PathTail::Complete,
    });
  }
}
