use flowistry::{
  extensions::{ContextMode, EVAL_MODE, EvalMode},
  infoflow::AnalysisSession,
};
use fluid_let::fluid_set;
use rustc_utils::{
  mir::borrowck_facts,
  source_map::{find_bodies::find_bodies, range::ToSpan},
  test_utils::CompileBuilder,
};

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
    fluid_set!(EVAL_MODE, mode);
    let (_, id) = find_bodies(tcx)
      .into_iter()
      .find(|(_, id)| {
        tcx
          .item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
          .as_str()
          == "main"
      })
      .unwrap();
    let session = AnalysisSession::new(tcx);
    let output = super::focus_with_session(tcx, id, session.clone()).unwrap();
    let snippet = |range: &rustc_utils::source_map::range::CharRange| {
      tcx
        .sess
        .source_map()
        .span_to_snippet(range.to_span(tcx).unwrap())
        .unwrap()
    };
    for (name, expected) in [("untouched", false), ("affected", true)] {
      let selected = output
        .place_info
        .iter()
        .filter(|place| snippet(&place.range) == name)
        .collect::<Vec<_>>();
      assert!(!selected.is_empty(), "missing focus target {name}");
      for place in selected {
        let slice = place
          .slice
          .iter()
          .map(&snippet)
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
              .map(&snippet)
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
    super::focus_with_session(tcx, id, session.clone()).unwrap();
    assert_eq!(session.stats().computations, 1);
    assert!(session.stats().cache_hits > 0);
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
    let (_, id) = find_bodies(tcx).into_iter().next().unwrap();
    let output = super::focus_with_session(tcx, id, AnalysisSession::new(tcx)).unwrap();
    let snippet = |range: &rustc_utils::source_map::range::CharRange| {
      tcx
        .sess
        .source_map()
        .span_to_snippet(range.to_span(tcx).unwrap())
        .unwrap()
    };
    let join = |ranges: &[rustc_utils::source_map::range::CharRange]| {
      ranges.iter().map(&snippet).collect::<Vec<_>>().join("\n")
    };
    let seen = output
      .place_info
      .iter()
      .find(|place| snippet(&place.range) == "seen")
      .expect("missing focus target seen");
    let (slice, maybe) = (join(&seen.slice), join(&seen.maybe_slice));
    assert!(!slice.contains("*a.borrow_mut() = input"), "slice: {slice}");
    assert!(maybe.contains("*a.borrow_mut() = input"), "maybe: {maybe}");
    assert!(maybe.contains("input = 17"), "maybe: {maybe}");

    let serialized = serde_json::to_value(&output).unwrap();
    assert!(
      serialized["place_info"]
        .as_array()
        .unwrap()
        .iter()
        .any(|place| place.get("maybe_slice").is_some())
    );
  });
}
