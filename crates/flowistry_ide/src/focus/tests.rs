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
fn source_selection_uses_compiler_parameters_and_comment_tokens() {
  let source = r###"
fn parameters<'a>(map: &'a Vec<i32>, mut saved: i32, (left, right): (i32, i32)) -> i32 {
  // enter_map_travel_screen is a comment, not a dependency
  let text = r#"// not a comment /* either */"#;
  /* outer /* nested */ café */ saved += map[0];
  let closure = |value: &i32| *value;
  saved + left + right + closure(&saved) + text.len() as i32
}
fn main() {}
"###;
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(source).compile(|result| {
    let tcx = result.tcx;
    let id = body_named(tcx, "parameters");
    let session = AnalysisSession::new(tcx, EvalMode::default());
    let output = super::focus_with_session(&session, id).unwrap();
    let aliases = output
      .parameter_aliases
      .iter()
      .map(|alias| {
        (
          snippet(tcx, &output.ranges[alias.range as usize]),
          snippet(tcx, &output.ranges[alias.target as usize]),
        )
      })
      .collect::<Vec<_>>();
    assert_eq!(aliases, vec![
      ("&'a Vec<i32>".into(), "map".into()),
      ("i32".into(), "saved".into())
    ]);
    let comments = output
      .comments
      .iter()
      .map(|index| snippet(tcx, &output.ranges[*index as usize]))
      .collect::<Vec<_>>();
    assert_eq!(comments, vec![
      "// enter_map_travel_screen is a comment, not a dependency",
      "/* outer /* nested */ café */"
    ]);
  });
}

#[test]
fn forward_constructor_focus_excludes_independent_fields() {
  let source = r#"
struct State { health: i32, timer: i32 }
fn restore(saved: &State) -> State {
  let state = State {
    health: saved.health,
    timer: 0,
  };
  state
}

fn main() {}
"#;
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(source).compile(|result| {
    let tcx = result.tcx;
    let session = AnalysisSession::new(tcx, EvalMode::default());
    let output = super::focus_with_session(&session, body_named(tcx, "restore")).unwrap();
    let selected = output
      .place_info
      .iter()
      .find(|place| snippet(tcx, &output.ranges[place.range as usize]) == "saved")
      .unwrap();
    let slice = selected
      .slice
      .iter()
      .map(|i| snippet(tcx, &output.ranges[*i as usize]))
      .collect::<Vec<_>>()
      .join("\n");
    assert!(slice.contains("saved.health"), "{slice}");
    assert!(!slice.contains("timer: 0"), "{slice}");
  });
}

#[test]
fn constructor_refinement_preserves_backward_effectful_and_controlled_inputs() {
  let source = r#"
struct State { health: i32, timer: i32, other: i32 }
fn effect() -> i32 { 5 }
fn reordered(saved: i32, other: i32) -> State {
  let state = State { other: other, timer: 0, health: saved };
  state
}
fn effectful(saved: i32) -> State {
  let state = State { health: saved, timer: effect(), other: 0 };
  state
}
fn controlled(flag: bool) -> State {
  let state = if flag { State { health: 1, timer: 2, other: 3 } }
    else { State { health: 4, timer: 5, other: 6 } };
  state
}
fn updated(saved: i32, base: State) -> State {
  let state = State { health: saved, timer: 0, ..base };
  state
}
fn main() {}
"#;
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(source).compile(|result| {
    let tcx = result.tcx;
    for context_mode in [ContextMode::SigOnly, ContextMode::Recurse] {
      let session = AnalysisSession::new(tcx, EvalMode {
        context_mode,
        ..EvalMode::default()
      });
      for (function, target, includes, excludes) in [
        ("reordered", "saved", vec!["health: saved"], vec![
          "timer: 0",
          "other: other",
        ]),
        (
          "reordered",
          "state",
          vec!["health: saved", "timer: 0", "other: other"],
          vec![],
        ),
        (
          "effectful",
          "saved",
          vec!["health: saved", "timer: effect()"],
          vec!["other: 0"],
        ),
        ("controlled", "flag", vec!["timer: 2", "timer: 5"], vec![]),
        ("updated", "saved", vec!["health: saved", "..base"], vec![
          "timer: 0",
        ]),
      ] {
        let output =
          super::focus_with_session(&session, body_named(tcx, function)).unwrap();
        let selected = output
          .place_info
          .iter()
          .filter(|place| snippet(tcx, &output.ranges[place.range as usize]) == target)
          .min_by_key(|place| output.ranges[place.range as usize].start)
          .unwrap();
        let slice = selected
          .slice
          .iter()
          .map(|i| snippet(tcx, &output.ranges[*i as usize]))
          .collect::<Vec<_>>()
          .join("\n");
        for text in includes {
          assert!(
            slice.contains(text),
            "{function}/{target}/{context_mode:?} missing {text}: {slice}"
          );
        }
        for text in excludes {
          assert!(
            !slice.contains(text),
            "{function}/{target}/{context_mode:?} includes {text}: {slice}"
          );
        }
      }
    }
  });
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
        .filter(|place| snippet(tcx, &output.ranges[place.range as usize]) == name)
        .collect::<Vec<_>>();
      assert!(!selected.is_empty(), "missing focus target {name}");
      for place in selected {
        let slice = place
          .slice
          .iter()
          .map(|range| snippet(tcx, &output.ranges[*range as usize]))
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
              .map(|range| snippet(tcx, &output.ranges[*range as usize]))
              .any(|text| text.contains("update"))
          );
        }
      }
    }
    let serialized = serde_json::to_value(&output).unwrap();
    assert_eq!(serialized.as_object().unwrap().len(), 3);
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
      let join = |ranges: &[u32]| {
        ranges
          .iter()
          .map(|range| snippet(tcx, &output.ranges[*range as usize]))
          .collect::<Vec<_>>()
          .join("\n")
      };
      let seen = output
        .place_info
        .iter()
        .find(|place| snippet(tcx, &output.ranges[place.range as usize]) == "seen")
        .expect("missing focus target seen");
      let (slice, maybe) = (join(&seen.slice), join(&seen.maybe_slice));
      assert!(!slice.contains("*a.borrow_mut() = input"), "slice: {slice}");
      assert!(maybe.contains("*a.borrow_mut() = input"), "maybe: {maybe}");
      assert!(maybe.contains("input = 17"), "maybe: {maybe}");

      // R6: the maybe slice is disjoint from the slice.
      for place in &output.place_info {
        for maybe in &place.maybe_slice {
          let maybe = output.ranges[*maybe as usize].to_span(tcx).unwrap();
          for exact in &place.slice {
            let exact = output.ranges[*exact as usize].to_span(tcx).unwrap();
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
