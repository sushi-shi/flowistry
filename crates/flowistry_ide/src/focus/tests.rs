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
