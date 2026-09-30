//! Identifies the mutated places in a MIR instruction via modular approximation based on types.

use log::debug;
use rustc_abi::FieldIdx;
use rustc_ast::InlineAsmOptions;
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{AdtKind, TyKind},
};
use rustc_span::Spanned;
use rustc_utils::{OperandExt, mir::place::PlaceCollector};

use super::{
  callsite::cmp_places_structurally,
  interior::{InteriorMutation, interior_mutable_places},
};
use crate::mir::{
  placeinfo::PlaceInfo,
  utils::{self, AsyncHack},
};

/// Indicator of certainty about whether a place is being mutated.
/// Used to determine whether an update should be strong or weak.
///
/// The status is derived from the [`MutationKind`] of a mutation, see
/// [`MutationKind::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationStatus {
  /// A place is definitely mutated, e.g. `x = y` definitely mutates `x`.
  Definitely,

  /// A place is possibly mutated, e.g. `f(&mut x)` possibly mutates `x`.
  Possibly,
}

/// What kind of write a [`Mutation`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MutationKind {
  /// `p = rvalue`, or the write of one field of a destructured aggregate or struct copy.
  Assign,

  /// The destination of a call is written with the call's return value.
  CallReturn,

  /// Modular approximation of a call: the place is mutably reachable from the
  /// operand at index `arg` of the call, so the callee may write it.
  CallArgument {
    /// Index of the call operand through which the place is reachable.
    arg: usize,
  },

  /// An output operand of an inline assembly block (`asm!`) is written from all of
  /// its inputs: the assembly itself is not analyzed.
  AsmOutput,

  /// Memory mutably reachable from the input operand at index `operand` of an inline
  /// assembly block may be written, unless the block is `nomem` or `readonly`.
  AsmMemory {
    /// Index of the assembly operand through which the place is reachable.
    operand: usize,
  },

  /// A dropped value may be written by a destructor of the source code in its drop
  /// glue, together with everything mutably reachable from it (in
  /// [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse) only).
  Destructor,

  /// An effect of a callee, translated from an analysis of the callee's body
  /// (see [`ContextMode::Recurse`](crate::extensions::ContextMode::Recurse)).
  CalleeEffect(CalleeEffect),
}

/// An effect of a callee on its caller, translated from the callee's analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalleeEffect {
  /// A write to (a part of) the call destination by the callee's return value.
  Return(Precision),

  /// A write through a pointer passed as an argument.
  ArgPointee(Precision),

  /// A write to the state shared through a handle passed by value as an argument,
  /// e.g. through an `Rc<RefCell<T>>`: the handle stands for its pointee.
  SharedState(Precision),
}

/// How precisely a callee effect is translated into a caller place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precision {
  /// The caller place is exactly the place the callee wrote.
  Exact,

  /// The callee wrote a part of the caller place that the caller cannot name
  /// (e.g. a private field), so the caller place is a coarser prefix of it.
  Coarsened,
}

impl MutationKind {
  /// Whether this kind of write definitely or only possibly mutates its place.
  pub fn status(self) -> MutationStatus {
    match self {
      MutationKind::Assign | MutationKind::CallReturn | MutationKind::AsmOutput => {
        MutationStatus::Definitely
      }
      MutationKind::CallArgument { .. }
      | MutationKind::AsmMemory { .. }
      | MutationKind::Destructor => MutationStatus::Possibly,
      MutationKind::CalleeEffect(effect) => match effect {
        CalleeEffect::Return(Precision::Exact) => MutationStatus::Definitely,
        // Several coarsened return effects can land on the same caller place, each
        // covering only a part of it: none of them overwrites the whole place.
        CalleeEffect::Return(Precision::Coarsened) => MutationStatus::Possibly,
        CalleeEffect::ArgPointee(Precision::Exact | Precision::Coarsened)
        | CalleeEffect::SharedState(Precision::Exact | Precision::Coarsened) => {
          MutationStatus::Possibly
        }
      },
    }
  }
}

/// Information about a particular mutation.
#[derive(Debug)]
pub struct Mutation<'tcx> {
  /// The place that is being mutated.
  pub mutated: Place<'tcx>,

  /// The set of inputs to the mutating operation.
  pub inputs: Vec<Place<'tcx>>,

  /// What kind of write this is.
  pub kind: MutationKind,
}

impl Mutation<'_> {
  /// The certainty of whether the mutation is happening, see [`MutationKind::status`].
  pub fn status(&self) -> MutationStatus {
    self.kind.status()
  }
}

/// MIR visitor that invokes a callback for every [`Mutation`] in the visited object.
///
/// Construct the visitor with [`ModularMutationVisitor::new`], then call one of the
/// MIR [`Visitor`] methods.
pub struct ModularMutationVisitor<'a, 'tcx, F>
where
  F: FnMut(Location, Vec<Mutation<'tcx>>),
{
  f: F,
  place_info: &'a PlaceInfo<'a, 'tcx>,
}

impl<'a, 'tcx, F> ModularMutationVisitor<'a, 'tcx, F>
where
  F: FnMut(Location, Vec<Mutation<'tcx>>),
{
  /// Constructs a new visitor.
  pub fn new(place_info: &'a PlaceInfo<'a, 'tcx>, f: F) -> Self {
    ModularMutationVisitor { place_info, f }
  }
}

impl<'tcx, F> Visitor<'tcx> for ModularMutationVisitor<'_, 'tcx, F>
where
  F: FnMut(Location, Vec<Mutation<'tcx>>),
{
  fn visit_assign(
    &mut self,
    mutated: &Place<'tcx>,
    rvalue: &Rvalue<'tcx>,
    location: Location,
  ) {
    debug!("Checking {location:?}: {mutated:?} = {rvalue:?}");
    let body = self.place_info.body;
    let tcx = self.place_info.tcx;

    match rvalue {
      // In the case of _1 = aggregate { field1: op1, field2: op2, ... },
      // then destructure this into a series of mutations like
      // _1.field1 = op1, _1.field2 = op2, and so on.
      Rvalue::Aggregate(agg_kind, ops) => {
        // A union aggregate `U { f: op }` has a single operand, for its active field.
        if let AggregateKind::Adt(def_id, idx, substs, _, Some(active_field)) =
          &**agg_kind
          && let [input_op] = &ops.raw[..]
        {
          let field_def = &tcx.adt_def(*def_id).variant(*idx).fields[*active_field];
          let field = PlaceElem::Field(*active_field, field_def.ty(tcx, substs));
          (self.f)(location, vec![Mutation {
            mutated: mutated.project_deeper(&[field], tcx),
            inputs: input_op.as_place().into_iter().collect(),
            kind: MutationKind::Assign,
          }]);
          return;
        }

        let info = match &**agg_kind {
          AggregateKind::Adt(def_id, idx, substs, _, _) => {
            let adt_def = tcx.adt_def(*def_id);
            let variant = adt_def.variant(*idx);
            let mutated = match adt_def.adt_kind() {
              AdtKind::Enum => mutated.project_deeper(
                &[ProjectionElem::Downcast(Some(variant.name), *idx)],
                tcx,
              ),
              AdtKind::Struct | AdtKind::Union => *mutated,
            };
            let fields = variant.fields.iter();
            let tys = fields
              .map(|field| field.ty(tcx, substs))
              .collect::<Vec<_>>();
            Some((mutated, tys))
          }
          AggregateKind::Tuple => {
            let ty = rvalue.ty(body.local_decls(), tcx);
            Some((*mutated, ty.tuple_fields().to_vec()))
          }
          _ => None,
        };

        if let Some((mutated, tys)) = info
          && tys.len() > 0
        {
          let fields =
            tys
              .into_iter()
              .enumerate()
              .zip(ops.iter())
              .map(|((i, ty), input_op)| {
                let field = PlaceElem::Field(FieldIdx::from_usize(i), ty);
                let input_place = input_op.as_place();
                (mutated.project_deeper(&[field], tcx), input_place)
              });

          let mutations = fields
            .map(|(mutated, input)| Mutation {
              mutated,
              inputs: input.into_iter().collect::<Vec<_>>(),
              kind: MutationKind::Assign,
            })
            .collect::<Vec<_>>();
          (self.f)(location, mutations);
          return;
        }
      }

      // In the case of _1 = _2 where _2 : struct Foo { x: T, y: S, .. },
      // then destructure this into a series of mutations like
      // _1.x = _2.x, _1.y = _2.y, and so on.
      Rvalue::Use(Operand::Move(place) | Operand::Copy(place)) => {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if let TyKind::Adt(adt_def, substs) = place_ty.kind()
          && adt_def.is_struct()
        {
          let fields = utils::visible_fields(*adt_def, self.place_info.def_id, tcx)
            .map(|(field, field_def)| PlaceElem::Field(field, field_def.ty(tcx, substs)));
          let mut mutations = fields
            .map(|field| {
              let mutated_field = mutated.project_deeper(&[field], tcx);
              let input_field = place.project_deeper(&[field], tcx);
              Mutation {
                mutated: mutated_field,
                inputs: vec![input_field],
                kind: MutationKind::Assign,
              }
            })
            .collect::<Vec<_>>();

          if mutations.is_empty() {
            mutations.push(Mutation {
              mutated: *mutated,
              inputs: vec![*place],
              kind: MutationKind::Assign,
            });
          }
          (self.f)(location, mutations);
          return;
        }
      }

      _ => {}
    }

    let mut collector = PlaceCollector::default();
    collector.visit_rvalue(rvalue, location);
    (self.f)(location, vec![Mutation {
      mutated: *mutated,
      inputs: collector.0,
      kind: MutationKind::Assign,
    }]);
  }

  fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, location: Location) {
    debug!("Checking {location:?}: {:?}", terminator.kind);
    let tcx = self.place_info.tcx;

    match &terminator.kind {
      TerminatorKind::Call {
        func,
        args,
        destination,
        ..
      } => {
        let CallArgumentWrites {
          inputs: arg_inputs,
          mutations: arg_mutations,
        } = call_argument_writes(
          self.place_info,
          args,
          |_| true,
          InteriorMutation::of_call(tcx, self.place_info.def_id, func),
        );

        let ret_is_unit = destination
          .ty(self.place_info.body.local_decls(), tcx)
          .ty
          .is_unit();
        let inputs = if ret_is_unit {
          Vec::new()
        } else {
          arg_inputs.clone()
        };

        if let Some((def_id, _)) = func.const_fn_def()
          && tcx.def_path_str(def_id) == "std::boxed::box_assume_init_into_vec_unsafe"
        {
          // TODO: we need to fix this case to fix vec_read...
        }

        let mut mutations = vec![Mutation {
          mutated: *destination,
          inputs,
          kind: MutationKind::CallReturn,
        }];
        mutations.extend(arg_mutations);

        (self.f)(location, mutations);
      }

      TerminatorKind::InlineAsm {
        operands, options, ..
      } => {
        (self.f)(
          location,
          inline_asm_writes(self.place_info, operands, *options),
        );
      }

      _ => {}
    }
  }
}

/// The writes of an inline assembly block (`asm!`), which is not analyzed: every
/// output is written from every input. Unless the block is declared `nomem`, it may
/// also read memory reachable from its inputs, and unless it is `nomem` or
/// `readonly`, it may write memory mutably reachable from them.
fn inline_asm_writes<'tcx>(
  place_info: &PlaceInfo<'_, 'tcx>,
  operands: &[InlineAsmOperand<'tcx>],
  options: InlineAsmOptions,
) -> Vec<Mutation<'tcx>> {
  let mut input_operands = Vec::new();
  let mut outputs = Vec::new();
  for (index, operand) in operands.iter().enumerate() {
    match operand {
      InlineAsmOperand::In { value, .. } => {
        input_operands.extend(value.as_place().map(|p| (index, p)))
      }
      InlineAsmOperand::InOut {
        in_value,
        out_place,
        ..
      } => {
        input_operands.extend(in_value.as_place().map(|p| (index, p)));
        outputs.extend(*out_place);
      }
      InlineAsmOperand::Out { place, .. } => outputs.extend(*place),
      InlineAsmOperand::Const { .. }
      | InlineAsmOperand::SymFn { .. }
      | InlineAsmOperand::SymStatic { .. }
      | InlineAsmOperand::Label { .. } => {}
    }
  }

  let mut inputs = input_operands.iter().map(|(_, p)| *p).collect::<Vec<_>>();
  if !options.contains(InlineAsmOptions::NOMEM) {
    for (_, input) in &input_operands {
      inputs.extend(
        place_info
          .reachable_values(*input, Mutability::Not)
          .iter()
          .copied(),
      );
    }
  }

  let mut mutations = outputs
    .into_iter()
    .map(|mutated| Mutation {
      mutated,
      inputs: inputs.clone(),
      kind: MutationKind::AsmOutput,
    })
    .collect::<Vec<_>>();
  if !options.intersects(InlineAsmOptions::NOMEM | InlineAsmOptions::READONLY) {
    for (operand, input) in &input_operands {
      let mut reachable = place_info
        .reachable_values(*input, Mutability::Mut)
        .iter()
        .copied()
        .filter(|place| place != input)
        .collect::<Vec<_>>();
      reachable.sort_by(|p1, p2| {
        cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
      });
      mutations.extend(reachable.into_iter().map(|mutated| Mutation {
        mutated,
        inputs: inputs.clone(),
        kind: MutationKind::AsmMemory { operand: *operand },
      }));
    }
  }
  mutations
}

/// The modular approximation of the writes of a call through its operands.
pub(crate) struct CallArgumentWrites<'tcx> {
  /// Every place operand of the call (except async contexts): the inputs of each
  /// write.
  pub inputs: Vec<Place<'tcx>>,
  /// The writes through the selected operands.
  pub mutations: Vec<Mutation<'tcx>>,
}

/// Computes the modular approximation of the writes of a call with operands `args`
/// through the operands whose index satisfies `operands`: the callee may write any
/// place mutably reachable from them, and, unless `interior` says it does not, the
/// interior-mutable state they give shared access to (see
/// [`interior_mutable_places`]), with every operand as an input.
///
/// Operands of the async [`Context`](std::task::Context) type are ignored (see
/// [`AsyncHack`]). The writes are ordered deterministically (see
/// [`cmp_places_structurally`]).
pub(crate) fn call_argument_writes<'tcx>(
  place_info: &PlaceInfo<'_, 'tcx>,
  args: &[Spanned<Operand<'tcx>>],
  operands: impl Fn(usize) -> bool,
  interior: InteriorMutation,
) -> CallArgumentWrites<'tcx> {
  let async_hack = AsyncHack::new(place_info.tcx, place_info.body, place_info.def_id);
  let arg_places = utils::arg_places(args)
    .into_iter()
    .filter(|(_, place)| !async_hack.ignore_place(*place))
    .collect::<Vec<_>>();
  let inputs = arg_places
    .iter()
    .map(|(_, place)| *place)
    .collect::<Vec<_>>();

  let mutations = arg_places
    .iter()
    .filter(|(i, _)| operands(*i))
    .flat_map(|(i, arg)| {
      let mut reachable = place_info
        .reachable_values(*arg, Mutability::Mut)
        .iter()
        .copied()
        .collect::<Vec<_>>();
      reachable.sort_by(|p1, p2| {
        cmp_places_structurally(p1.local, p1.projection, p2.local, p2.projection)
      });
      match interior {
        InteriorMutation::Possible => {
          reachable.extend(interior_mutable_places(place_info, *arg))
        }
        InteriorMutation::None => {}
      }
      let inputs = &inputs;
      reachable.into_iter().map(move |mutated| Mutation {
        mutated,
        inputs: inputs.clone(),
        kind: MutationKind::CallArgument { arg: *i },
      })
    })
    .collect();

  CallArgumentWrites { inputs, mutations }
}

#[cfg(test)]
mod test {
  use rustc_middle::ty::TyCtxt;
  use rustc_utils::{BodyExt, test_utils::Placer};

  use super::*;
  use crate::test_utils;

  #[test]
  fn test_mutation_kinds() {
    let input = r#"
struct S { a: i32, b: i32 }
fn f(g: fn(&mut i32, i32) -> i32, s: S) {
  let t = (1, 2);
  let u = S { a: 1, b: 2 };
  let v = s;
  let w = t.0 + 1;
  let mut x = 0;
  let r = g(&mut x, w);
}
"#;
    test_utils::compile_body(input, check_mutation_kinds);
  }

  fn check_mutation_kinds<'tcx>(
    tcx: TyCtxt<'tcx>,
    body_id: rustc_hir::BodyId,
    body_with_facts: &rustc_borrowck::consumers::BodyWithBorrowckFacts<'tcx>,
  ) {
    let body = &body_with_facts.body;
    let def_id = tcx.hir_body_owner_def_id(body_id).to_def_id();
    let place_info = PlaceInfo::build(tcx, def_id, body_with_facts);
    let p = Placer::new(tcx, body);

    let mut mutations = Vec::new();
    let mut visitor = ModularMutationVisitor::new(&place_info, |_, mts| {
      mutations.extend(mts.into_iter().map(|mt| (mt.mutated, mt.kind)));
    });
    for location in body.all_locations() {
      visitor.visit_location(body, location);
    }

    let kinds_of = |place: Place<'tcx>| {
      mutations
        .iter()
        .filter(|(mutated, _)| *mutated == place)
        .map(|(_, kind)| *kind)
        .collect::<Vec<_>>()
    };

    // Tuple and struct aggregates and struct copies: one Assign per field.
    for local in ["t", "u", "v"] {
      for field in 0 .. 2 {
        let place = p.local(local).field(field).mk();
        assert_eq!(kinds_of(place), vec![MutationKind::Assign], "{place:?}");
      }
    }
    // Generic assignment.
    assert_eq!(kinds_of(p.local("w").mk()), vec![MutationKind::Assign]);
    // Call destination.
    assert_eq!(kinds_of(p.local("r").mk()), vec![MutationKind::CallReturn]);
    // `x` is mutably reachable from the first operand of the call.
    assert_eq!(kinds_of(p.local("x").mk()), vec![
      MutationKind::Assign,
      MutationKind::CallArgument { arg: 0 }
    ]);
    // The second operand (a copy of `w`) is reachable from itself only.
    assert_eq!(
      mutations
        .iter()
        .filter(|(_, kind)| *kind == MutationKind::CallArgument { arg: 1 })
        .count(),
      1
    );
  }

  #[test]
  fn test_struct_copy_uses_real_field_indices() {
    let input = r#"
mod m { pub struct S { a: u8, pub b: i32 } }
fn f(s: m::S) { let t = s; }
"#;
    test_utils::compile_body(input, check_struct_copy);
  }

  fn check_struct_copy<'tcx>(
    tcx: TyCtxt<'tcx>,
    body_id: rustc_hir::BodyId,
    body_with_facts: &rustc_borrowck::consumers::BodyWithBorrowckFacts<'tcx>,
  ) {
    let body = &body_with_facts.body;
    let def_id = tcx.hir_body_owner_def_id(body_id).to_def_id();
    let place_info = PlaceInfo::build(tcx, def_id, body_with_facts);
    let p = Placer::new(tcx, body);
    let t = p.local("t").mk();

    let mut writes = Vec::new();
    let mut visitor = ModularMutationVisitor::new(&place_info, |_, mts| {
      writes.extend(mts.into_iter().map(|mt| (mt.mutated, mt.inputs)));
    });
    for location in body.all_locations() {
      visitor.visit_location(body, location);
    }
    let t_writes = writes
      .into_iter()
      .filter(|(mutated, _)| mutated.local == t.local)
      .collect::<Vec<_>>();

    // Only `b`, the field at index 1, is visible, and it is copied from `s.b`.
    assert_eq!(t_writes, vec![(p.local("t").field(1).mk(), vec![
      p.local("s").field(1).mk()
    ])]);
  }

  #[test]
  fn test_union_aggregate_writes_active_field() {
    let input = r#"
union U { a: u8, b: i32 }
fn f(x: i32) { let u = U { b: x }; }
"#;
    test_utils::compile_body(input, check_union_aggregate);
  }

  fn check_union_aggregate<'tcx>(
    tcx: TyCtxt<'tcx>,
    body_id: rustc_hir::BodyId,
    body_with_facts: &rustc_borrowck::consumers::BodyWithBorrowckFacts<'tcx>,
  ) {
    let body = &body_with_facts.body;
    let def_id = tcx.hir_body_owner_def_id(body_id).to_def_id();
    let place_info = PlaceInfo::build(tcx, def_id, body_with_facts);
    let p = Placer::new(tcx, body);
    let u = p.local("u").mk();

    let mut writes = Vec::new();
    let mut visitor = ModularMutationVisitor::new(&place_info, |_, mts| {
      writes.extend(mts.into_iter().map(|mt| (mt.mutated, mt.kind)));
    });
    for location in body.all_locations() {
      visitor.visit_location(body, location);
    }
    let u_writes = writes
      .into_iter()
      .filter(|(mutated, _)| mutated.local == u.local)
      .collect::<Vec<_>>();

    // Only the active field `b` (index 1, of type i32) is written.
    let b = tcx.mk_place_field(u, FieldIdx::from_usize(1), tcx.types.i32);
    assert_eq!(u_writes, vec![(b, MutationKind::Assign)]);
  }

  #[test]
  fn test_mutation_kind_status() {
    use CalleeEffect::*;
    use MutationStatus::*;
    use Precision::*;
    let cases = [
      (MutationKind::Assign, Definitely),
      (MutationKind::CallReturn, Definitely),
      (MutationKind::CallArgument { arg: 3 }, Possibly),
      (MutationKind::AsmOutput, Definitely),
      (MutationKind::AsmMemory { operand: 0 }, Possibly),
      (MutationKind::CalleeEffect(Return(Exact)), Definitely),
      (MutationKind::CalleeEffect(Return(Coarsened)), Possibly),
      (MutationKind::CalleeEffect(ArgPointee(Exact)), Possibly),
      (MutationKind::CalleeEffect(ArgPointee(Coarsened)), Possibly),
      (MutationKind::CalleeEffect(SharedState(Exact)), Possibly),
      (MutationKind::Destructor, Possibly),
    ];
    for (kind, status) in cases {
      assert_eq!(kind.status(), status, "{kind:?}");
    }
  }

  #[test]
  fn test_inline_asm_writes_respect_memory_options() {
    for (options, expect_memory_writes) in [
      ("nostack", true),
      ("readonly, nostack", false),
      ("nomem, nostack", false),
    ] {
      let input = format!(
        r#"
fn f(p: *mut usize) -> usize {{
  let r: usize;
  unsafe {{ std::arch::asm!("/* {{r}} {{p}} */", p = in(reg) p, r = lateout(reg) r, options({options})); }}
  r
}}
"#
      );
      test_utils::compile_body(input, move |tcx, body_id, body_with_facts| {
        let body = &body_with_facts.body;
        let def_id = tcx.hir_body_owner_def_id(body_id).to_def_id();
        let place_info = PlaceInfo::build(tcx, def_id, body_with_facts);
        let r = Placer::new(tcx, body).local("r").mk();

        let mut kinds = Vec::new();
        let mut visitor = ModularMutationVisitor::new(&place_info, |_, mts| {
          kinds.extend(mts.into_iter().map(|mt| (mt.mutated, mt.kind)));
        });
        for location in body.all_locations() {
          visitor.visit_location(body, location);
        }

        assert!(
          kinds.contains(&(r, MutationKind::AsmOutput)),
          "{options}: {kinds:?}"
        );
        let memory_writes = kinds
          .iter()
          .any(|(_, kind)| matches!(kind, MutationKind::AsmMemory { .. }));
        assert_eq!(memory_writes, expect_memory_writes, "{options}: {kinds:?}");
      });
    }
  }
}
