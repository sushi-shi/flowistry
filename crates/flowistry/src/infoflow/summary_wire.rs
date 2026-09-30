//! Portable *logical* summaries, independent of physical dataflow rows.
//!
//! The only rustc indices converted here are structural field/variant ordinals.
//! They are interpreted against the current, fingerprint-validated callee ABI
//! and types. No DefId, Local, Ty, Span, location or interner slot is serialized.
//! Keys, compiler validation, checksums and byte budgets belong to the store.

use std::rc::Rc;

use rustc_abi::{FieldIdx, VariantIdx};
use serde::{Deserialize, Serialize};

use super::{
  callsite::{
    ArgPos, CalleeAbi, EffectPath, EffectRoot, FallbackReason, PathElem, PathTail,
  },
  summary::{CalleeSummary, EffectKind, InputContents, SummaryEffect, SummaryInput},
};

const SCHEMA: u32 = 1;

/// Portable summary payload. Only a compiler-validated store may supply it to a
/// session. Deserialization alone does not establish semantic validity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableSummary {
  schema: u32,
  abi: Abi,
  outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Abi {
  Direct { arguments: u32 },
  Closure { tuple_fields: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Outcome {
  Summary {
    origins: Vec<Input>,
    reads: Vec<u32>,
    whole_return_inputs: Vec<u32>,
    effects: Vec<Effect>,
    opaque_operands: Vec<u32>,
  },
  Fallback(FallbackReason),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
  path: Path,
  contents: InputContents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Effect {
  kind: EffectKind,
  path: Path,
  inputs: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Root {
  Return,
  Argument(u32),
  TupleArgument { operand: u32, field: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Element {
  Deref,
  Field(u32),
  Downcast(u32),
  AnyIndex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Tail {
  Complete,
  Truncated { dropped_deref: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Path {
  root: Root,
  elements: Vec<Element>,
  tail: Tail,
}

impl Abi {
  fn capture(abi: CalleeAbi) -> Option<Self> {
    Some(match abi {
      CalleeAbi::Direct { arg_count } => Self::Direct {
        arguments: arg_count.try_into().ok()?,
      },
      CalleeAbi::ClosureBody { untupled } => Self::Closure {
        tuple_fields: untupled.try_into().ok()?,
      },
    })
  }

  fn operands(&self) -> u32 {
    match *self {
      Self::Direct { arguments } => arguments,
      Self::Closure { .. } => 2,
    }
  }

  fn root(&self, root: &Root) -> Option<EffectRoot> {
    Some(match (self, root) {
      (_, Root::Return) => EffectRoot::Return,
      (Self::Direct { arguments }, Root::Argument(i)) if i < arguments => {
        EffectRoot::Arg(ArgPos::Plain(*i as usize))
      }
      (Self::Closure { .. }, Root::Argument(0)) => EffectRoot::Arg(ArgPos::Plain(0)),
      (Self::Closure { tuple_fields }, Root::TupleArgument { operand: 1, field })
        if field < tuple_fields && *field <= FieldIdx::MAX_AS_U32 =>
      {
        EffectRoot::Arg(ArgPos::Tupled {
          tuple: 1,
          field: FieldIdx::from_u32(*field),
        })
      }
      _ => return None,
    })
  }
}

impl Path {
  fn capture(path: &EffectPath) -> Option<Self> {
    Some(Self {
      root: match path.root {
        EffectRoot::Return => Root::Return,
        EffectRoot::Arg(ArgPos::Plain(i)) => Root::Argument(i.try_into().ok()?),
        EffectRoot::Arg(ArgPos::Tupled { tuple, field }) => Root::TupleArgument {
          operand: tuple.try_into().ok()?,
          field: field.as_u32(),
        },
      },
      elements: path
        .elems
        .iter()
        .map(|elem| match elem {
          PathElem::Deref => Element::Deref,
          PathElem::Field(field) => Element::Field(field.as_u32()),
          PathElem::Downcast(variant) => Element::Downcast(variant.as_u32()),
          PathElem::AnyIndex => Element::AnyIndex,
        })
        .collect(),
      tail: match path.tail {
        PathTail::Complete => Tail::Complete,
        PathTail::Truncated { dropped_deref } => Tail::Truncated { dropped_deref },
      },
    })
  }

  fn restore(&self, abi: &Abi) -> Option<EffectPath> {
    Some(EffectPath {
      root: abi.root(&self.root)?,
      elems: self
        .elements
        .iter()
        .map(|elem| {
          Some(match *elem {
            Element::Deref => PathElem::Deref,
            Element::Field(i) if i <= FieldIdx::MAX_AS_U32 => {
              PathElem::Field(FieldIdx::from_u32(i))
            }
            Element::Downcast(i) if i <= VariantIdx::MAX_AS_U32 => {
              PathElem::Downcast(VariantIdx::from_u32(i))
            }
            Element::AnyIndex => PathElem::AnyIndex,
            _ => return None,
          })
        })
        .collect::<Option<_>>()?,
      tail: match self.tail {
        Tail::Complete => PathTail::Complete,
        Tail::Truncated { dropped_deref } => PathTail::Truncated { dropped_deref },
      },
    })
  }
}

fn indices(values: &[usize]) -> Option<Vec<u32>> {
  values.iter().map(|&i| i.try_into().ok()).collect()
}

fn restore_indices(values: &[u32], bound: usize) -> Option<Vec<usize>> {
  values
    .iter()
    .map(|&i| ((i as usize) < bound).then_some(i as usize))
    .collect()
}

impl PortableSummary {
  pub(crate) fn capture(
    abi: CalleeAbi,
    summary: &Result<Rc<CalleeSummary>, FallbackReason>,
  ) -> Option<Self> {
    let outcome = match summary {
      Err(reason) => Outcome::Fallback(*reason),
      Ok(summary) => {
        if summary.abi != abi {
          return None;
        }
        Outcome::Summary {
          origins: summary
            .origins
            .iter()
            .map(|origin| {
              Some(Input {
                path: Path::capture(&origin.path)?,
                contents: origin.contents,
              })
            })
            .collect::<Option<_>>()?,
          reads: indices(&summary.reads)?,
          whole_return_inputs: indices(&summary.whole_return_inputs)?,
          effects: summary
            .effects
            .iter()
            .map(|effect| {
              Some(Effect {
                kind: effect.kind,
                path: Path::capture(&effect.path)?,
                inputs: indices(&effect.inputs)?,
              })
            })
            .collect::<Option<_>>()?,
          opaque_operands: indices(&summary.opaque_operands)?,
        }
      }
    };
    Some(Self {
      schema: SCHEMA,
      abi: Abi::capture(abi)?,
      outcome,
    })
  }

  pub(crate) fn restore(
    &self,
    abi: CalleeAbi,
  ) -> Option<Result<Rc<CalleeSummary>, FallbackReason>> {
    if self.schema != SCHEMA || self.abi != Abi::capture(abi)? {
      return None;
    }
    Some(match &self.outcome {
      Outcome::Fallback(reason) => Err(*reason),
      Outcome::Summary {
        origins,
        reads,
        whole_return_inputs,
        effects,
        opaque_operands,
      } => {
        let bound = origins.len();
        let origins = origins
          .iter()
          .map(|origin| {
            let path = origin.path.restore(&self.abi)?;
            if matches!(path.root, EffectRoot::Return) {
              return None;
            }
            Some(SummaryInput {
              path,
              contents: origin.contents,
            })
          })
          .collect::<Option<Vec<_>>>()?;
        let effects = effects
          .iter()
          .map(|effect| {
            let path = effect.path.restore(&self.abi)?;
            if (effect.kind == EffectKind::Return)
              != matches!(path.root, EffectRoot::Return)
            {
              return None;
            }
            Some(SummaryEffect {
              path,
              kind: effect.kind,
              inputs: restore_indices(&effect.inputs, bound)?,
            })
          })
          .collect::<Option<_>>()?;
        Ok(Rc::new(CalleeSummary {
          abi,
          origins,
          effects,
          reads: restore_indices(reads, bound)?,
          whole_return_inputs: restore_indices(whole_return_inputs, bound)?,
          opaque_operands: restore_indices(
            opaque_operands,
            self.abi.operands() as usize,
          )?
          .into_iter()
          .collect(),
        }))
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn example() -> Result<Rc<CalleeSummary>, FallbackReason> {
    let argument = EffectPath {
      root: EffectRoot::Arg(ArgPos::Tupled {
        tuple: 1,
        field: FieldIdx::from_u32(2),
      }),
      elems: [
        PathElem::Deref,
        PathElem::Downcast(VariantIdx::from_u32(3)),
        PathElem::Field(FieldIdx::from_u32(4)),
        PathElem::AnyIndex,
      ]
      .into_iter()
      .collect(),
      tail: PathTail::Truncated {
        dropped_deref: true,
      },
    };
    let destination = EffectPath {
      root: EffectRoot::Return,
      elems: Default::default(),
      tail: PathTail::Complete,
    };
    Ok(Rc::new(CalleeSummary {
      abi: CalleeAbi::ClosureBody { untupled: 3 },
      origins: vec![
        SummaryInput {
          path: argument.clone(),
          contents: InputContents::Address,
        },
        SummaryInput {
          path: argument.clone(),
          contents: InputContents::Reachable,
        },
      ],
      reads: vec![0, 1],
      whole_return_inputs: vec![1],
      effects: vec![
        SummaryEffect {
          kind: EffectKind::Return,
          path: destination,
          inputs: vec![1, 0],
        },
        SummaryEffect {
          kind: EffectKind::ArgPointee,
          path: argument.clone(),
          inputs: vec![0],
        },
        SummaryEffect {
          kind: EffectKind::SharedState,
          path: argument,
          inputs: vec![1],
        },
      ],
      opaque_operands: [0, 1].into_iter().collect(),
    }))
  }

  #[test]
  fn preserves_logical_summary_and_fallbacks() {
    let abi = CalleeAbi::ClosureBody { untupled: 3 };
    let summary = example();
    let wire = PortableSummary::capture(abi, &summary).unwrap();
    assert_eq!(wire.restore(abi).unwrap(), summary);
    let fallback = Err(FallbackReason::UnsupportedBody(
      super::super::UnsupportedOp::RawPointer,
    ));
    assert_eq!(
      PortableSummary::capture(abi, &fallback)
        .unwrap()
        .restore(abi)
        .unwrap(),
      fallback
    );
  }

  #[test]
  fn rejects_schema_abi_and_dangling_origins() {
    let abi = CalleeAbi::ClosureBody { untupled: 3 };
    let mut wire = PortableSummary::capture(abi, &example()).unwrap();
    assert!(wire.restore(CalleeAbi::Direct { arg_count: 3 }).is_none());
    wire.schema += 1;
    assert!(wire.restore(abi).is_none());
    wire.schema = SCHEMA;
    if let Outcome::Summary { reads, .. } = &mut wire.outcome {
      reads.push(2);
    }
    assert!(wire.restore(abi).is_none());
  }

  #[test]
  fn rejects_invalid_structural_ordinals_and_roots() {
    let abi = Abi::Closure { tuple_fields: 3 };
    let path = Path::capture(&example().unwrap().origins[0].path).unwrap();
    let mut bad = path.clone();
    bad.elements.push(Element::Field(u32::MAX));
    assert!(bad.restore(&abi).is_none());
    bad = path.clone();
    bad.elements.push(Element::Downcast(u32::MAX));
    assert!(bad.restore(&abi).is_none());
    bad = path;
    bad.root = Root::TupleArgument {
      operand: 1,
      field: 3,
    };
    assert!(bad.restore(&abi).is_none());
    bad.root = Root::Argument(1);
    assert!(bad.restore(&abi).is_none());
  }

  #[test]
  fn compiler_summaries_roundtrip_across_sessions() {
    use std::{
      collections::BTreeMap,
      sync::{Arc, Mutex},
    };

    use rustc_hir::def_id::LocalDefId;

    use crate::{
      extensions::{ContextMode, EvalMode},
      infoflow::{AnalysisSession, SummaryStore},
      test_utils,
    };

    // This test's source and mode are fixed. The real store must additionally
    // validate semantic inputs; names are sufficient only for this wire test.
    #[derive(Clone, Default)]
    struct MemoryStore(Arc<Mutex<BTreeMap<String, Vec<u8>>>>);
    impl<'tcx> SummaryStore<'tcx> for MemoryStore {
      fn load(
        &self,
        session: &AnalysisSession<'tcx>,
        callee: LocalDefId,
      ) -> Option<PortableSummary> {
        let bytes = self
          .0
          .lock()
          .unwrap()
          .get(&session.tcx().def_path_str(callee))?
          .clone();
        serde_json::from_slice(&bytes).ok()
      }
      fn save(
        &self,
        session: &AnalysisSession<'tcx>,
        callee: LocalDefId,
        wire: &PortableSummary,
      ) {
        self.0.lock().unwrap().insert(
          session.tcx().def_path_str(callee),
          serde_json::to_vec(wire).unwrap(),
        );
      }
    }
    let source = r#"
use std::{rc::Rc, cell::RefCell};
fn leaf(x: &mut (i32, i32), y: i32) { x.0 = y; }
fn cyclic(x: u32) -> u32 { if x == 0 { x } else { cyclic(x - 1) } }
fn shared(x: Rc<RefCell<i32>>, y: i32) { *x.borrow_mut() = y; }
fn opaque(x: &mut i32) { unsafe { *(x as *mut i32) = 17; } }
fn caller(x: &mut (i32, i32), y: i32) {
  leaf(x, y);
  let c = |p: &mut i32| { *p = y; };
  c(&mut x.1);
}
"#;
    let store = MemoryStore::default();
    for phase in 0 .. 4 {
      if phase == 2 {
        // Valid JSON with an obsolete schema must cause computation, not a hit.
        for bytes in store.0.lock().unwrap().values_mut() {
          let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
          value["schema"] = (SCHEMA + 1).into();
          *bytes = serde_json::to_vec(&value).unwrap();
        }
      }
      let store = store.clone();
      test_utils::compile_crate(source, &[], move |tcx| {
        let mode = EvalMode {
          context_mode: ContextMode::Recurse,
          ..EvalMode::default()
        };
        let session = AnalysisSession::with_summary_store(tcx, mode, Rc::new(store));
        let fresh = AnalysisSession::new(tcx, mode);
        let mut defs = tcx
          .hir_body_owners()
          .filter(|&def| tcx.hir_body_owner_kind(def).is_fn_or_closure())
          .collect::<Vec<_>>();
        defs.sort_by_key(|&def| tcx.def_path_str(def));
        for def in &defs {
          let actual = session.summary(*def);
          assert_eq!(actual, fresh.summary(*def), "{}", tcx.def_path_str(*def));
          let abi = CalleeAbi::of_body(tcx, def.to_def_id(), &session.body(*def).body);
          let wire = PortableSummary::capture(abi, &actual).unwrap();
          let bytes = serde_json::to_vec(&wire).unwrap();
          let decoded: PortableSummary = serde_json::from_slice(&bytes).unwrap();
          assert_eq!(decoded.restore(abi).unwrap(), actual);
        }
        let stats = session.stats();
        if phase == 1 || phase == 3 {
          assert_eq!(stats.computations, 0, "{stats:?}");
          assert_eq!(stats.persistent_hits, defs.len(), "{stats:?}");
        } else {
          assert_eq!(stats.computations, defs.len(), "{stats:?}");
          assert_eq!(stats.persistent_hits, 0, "{stats:?}");
          assert_eq!(stats.persistent_misses, defs.len(), "{stats:?}");
        }
      });
    }
  }
}
