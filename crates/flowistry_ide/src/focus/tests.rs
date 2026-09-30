use flowistry::{
  extensions::{ContextMode, EvalMode},
  infoflow::AnalysisSession,
};
use rustc_middle::ty::TyCtxt;
use rustc_utils::{
  mir::borrowck_facts,
  source_map::{
    find_bodies::find_bodies,
    range::{CharRange, ToSpan},
  },
  test_utils::CompileBuilder,
};

fn body_named(tcx: TyCtxt<'_>, name: &str) -> rustc_hir::BodyId {
  find_bodies(tcx)
    .into_iter()
    .map(|(_, id)| id)
    .find(|id| {
      tcx
        .opt_item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
        .is_some_and(|item| item.as_str() == name)
    })
    .unwrap()
}

fn snippet(tcx: TyCtxt<'_>, range: &CharRange) -> String {
  tcx
    .sess
    .source_map()
    .span_to_snippet(range.to_span(tcx).unwrap())
    .unwrap()
}

#[test]
fn focus_output_preserves_protocol_and_field_precision() {
  let source = r#"
struct State { a: i32, b: i32 }
impl State { fn update(&mut self, input: i32) { self.b = input; } }
fn main() {
 let mut s = State { a: 1, b: 2 };
 let input = 17;
 s.update(input);
 let untouched = s.a;
 let affected = s.b;
}
"#;
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(source).compile(|result| {
    let tcx = result.tcx;
    let mode = EvalMode {
      context_mode: ContextMode::Recurse,
      ..EvalMode::default()
    };
    let id = body_named(tcx, "main");
    let session = AnalysisSession::new(tcx, mode);
    let output = super::focus_with_session(&session, id).unwrap();
    for (name, expected) in [("untouched", false), ("affected", true)] {
      let selected = output
        .place_info
        .iter()
        .filter(|place| snippet(tcx, &place.range) == name)
        .collect::<Vec<_>>();
      assert!(!selected.is_empty(), "missing focus target {name}");
      for place in selected {
        let slice = place
          .slice
          .iter()
          .map(|range| snippet(tcx, range))
          .collect::<Vec<_>>()
          .join("\n");
        assert_eq!(
          slice.contains("s.update(input)"),
          expected,
          "{name}: {slice}"
        );
        if !expected {
          assert!(
            !place
              .direct_influence
              .iter()
              .map(|range| snippet(tcx, range))
              .any(|text| text.contains("update"))
          );
        }
      }
    }
    let serialized = serde_json::to_value(&output).unwrap();
    assert_eq!(serialized.as_object().unwrap().len(), 2);
    for place in serialized["place_info"].as_array().unwrap() {
      let fields = place.as_object().unwrap();
      assert_eq!(fields.len(), 4);
      for field in ["range", "ranges", "slice", "direct_influence"] {
        assert!(fields.contains_key(field));
      }
    }

    // The summary of `update` is computed once for the session.
    super::focus_with_session(&session, id).unwrap();
    let stats = session.stats();
    assert_eq!(stats.computations, 1, "{stats:?}");
    assert!(stats.cache_hits > 0, "{stats:?}");
  });
}

#[test]
fn focus_output_marks_shared_handle_writes_as_maybe() {
  let source = r#"
use std::{cell::RefCell, rc::Rc};
fn main() {
 let a = Rc::new(RefCell::new(0));
 let b = a.clone();
 let input = 17;
 *a.borrow_mut() = input;
 let seen = *b.borrow();
}
"#;
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(source).compile(|result| {
    let tcx = result.tcx;
    for context_mode in [ContextMode::SigOnly, ContextMode::Recurse] {
      let mode = EvalMode {
        context_mode,
        ..EvalMode::default()
      };
      let session = AnalysisSession::new(tcx, mode);
      let output = super::focus_with_session(&session, body_named(tcx, "main")).unwrap();
      let join = |ranges: &[CharRange]| {
        ranges
          .iter()
          .map(|range| snippet(tcx, range))
          .collect::<Vec<_>>()
          .join("\n")
      };
      let seen = output
        .place_info
        .iter()
        .find(|place| snippet(tcx, &place.range) == "seen")
        .expect("missing focus target seen");
      let (slice, maybe) = (join(&seen.slice), join(&seen.maybe_slice));
      assert!(!slice.contains("*a.borrow_mut() = input"), "slice: {slice}");
      assert!(maybe.contains("*a.borrow_mut() = input"), "maybe: {maybe}");
      assert!(maybe.contains("input = 17"), "maybe: {maybe}");

      // R6: the maybe slice is disjoint from the slice.
      for place in &output.place_info {
        for maybe in &place.maybe_slice {
          let maybe = maybe.to_span(tcx).unwrap();
          for exact in &place.slice {
            let exact = exact.to_span(tcx).unwrap();
            assert!(
              !maybe.overlaps(exact),
              "{context_mode:?}: {maybe:?} overlaps {exact:?}"
            );
          }
        }
      }

      // `maybe_slice` is only serialized when there is one.
      let serialized = serde_json::to_value(&output).unwrap();
      let places = serialized["place_info"].as_array().unwrap();
      assert!(
        places
          .iter()
          .any(|place| place.get("maybe_slice").is_some())
      );
      assert!(
        places
          .iter()
          .any(|place| place.get("maybe_slice").is_none())
      );
    }
  });
}

/// R6: the parts of the maybe slice that the exact slice covers are removed, also
/// when a maybe span contains or partially overlaps an exact span.
#[test]
fn maybe_spans_are_subtracted_from_exact_spans() {
  use rustc_span::{BytePos, Span};
  let span = |lo: u32, hi: u32| Span::with_root_ctxt(BytePos(lo), BytePos(hi));
  rustc_span::create_default_session_globals_then(|| {
    // Contained: the maybe span keeps what surrounds the exact span.
    assert_eq!(super::subtract_spans(&[span(0, 20)], &[span(5, 10)]), vec![
      span(0, 5),
      span(10, 20)
    ]);
    // Partially overlapping, on either side.
    assert_eq!(super::subtract_spans(&[span(0, 10)], &[span(5, 15)]), vec![
      span(0, 5)
    ]);
    assert_eq!(super::subtract_spans(&[span(5, 15)], &[span(0, 10)]), vec![
      span(10, 15)
    ]);
    // Covered.
    assert!(super::subtract_spans(&[span(5, 8)], &[span(0, 10)]).is_empty());
    // Disjoint.
    assert_eq!(
      super::subtract_spans(&[span(20, 30)], &[span(0, 10)]),
      vec![span(20, 30)]
    );
  });
}
