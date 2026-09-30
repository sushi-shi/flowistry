//! Helpers shared by the integration tests that slice small programs.

#![allow(dead_code)]

use flowistry::{
  extensions::{ContextMode, EvalMode},
  infoflow::{self, AnalysisSession, Direction},
  test_utils,
};
use rustc_utils::source_map::{range::ToSpan, spanner::Spanner};

/// The source snippets of the slice of the `` `(target)` `` of `input`, one per line,
/// computed in `mode` (with incremental compilation if `args` asks for it).
pub fn slice_with_args(
  input: &str,
  mode: ContextMode,
  direction: Direction,
  args: &[String],
) -> String {
  let input = input.to_owned();
  let (clean, _) = test_utils::parse_ranges(&input, [("`(", ")`")]).unwrap();
  let output = std::sync::Mutex::new(String::new());
  test_utils::compile_body_with_range_and_args(
    clean,
    || test_utils::parse_ranges(&input, [("`(", ")`")]).unwrap().1["`("][0],
    args,
    |tcx, body_id, facts, target| {
      let mode = EvalMode {
        context_mode: mode,
        ..EvalMode::default()
      };
      let session = AnalysisSession::new(tcx, mode);
      let results = infoflow::compute_flow_with_session(&session, body_id, facts);
      let spanner = Spanner::new(tcx, body_id, &facts.body);
      let targets = spanner
        .span_to_places(target.to_span(tcx).unwrap())
        .iter()
        .map(|span| {
          span
            .locations
            .iter()
            .map(|location| (span.place, *location))
            .collect()
        })
        .collect();
      let spans = match direction {
        Direction::Both => infoflow::compute_focus_spans(&results, targets, &spanner),
        _ => infoflow::compute_dependency_spans(&results, targets, direction, &spanner),
      };
      let mut snippets = spans
        .iter()
        .flatten()
        .map(|span| tcx.sess.source_map().span_to_snippet(*span).unwrap())
        .collect::<Vec<_>>();
      snippets.sort();
      snippets.dedup();
      *output.lock().unwrap() = snippets.join("\n");
    },
  );
  output.into_inner().unwrap()
}

/// [`slice_with_args`] without extra compiler arguments.
pub fn slice(input: &str, mode: ContextMode, direction: Direction) -> String {
  slice_with_args(input, mode, direction, &[])
}

/// Checks that the slice of `input` in `Recurse` mode contains each of `included`
/// and none of `excluded`.
pub fn check_recurse(
  input: &str,
  direction: Direction,
  included: &[&str],
  excluded: &[&str],
) {
  let snippets = slice(input, ContextMode::Recurse, direction);
  for text in included {
    assert!(snippets.contains(text), "missing {text:?} in:\n{snippets}");
  }
  for text in excluded {
    assert!(
      !snippets.contains(text),
      "unexpected {text:?} in:\n{snippets}"
    );
  }
}

/// Whether the backward slice of `input` in `mode` contains `marker`.
pub fn sees(input: &str, mode: ContextMode, marker: &str) -> bool {
  slice(input, mode, Direction::Backward).contains(marker)
}
