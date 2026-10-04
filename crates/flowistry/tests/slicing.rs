#![feature(rustc_private)]

extern crate rustc_span;

use flowistry::{
  infoflow::{self, Direction},
  test_utils,
};
use rustc_span::Span;
use rustc_utils::SpanExt;
use test_log::test;

fn slice(dir: &str, direction: Direction) {
  test_utils::run_tests(dir, |path, expected| {
    test_utils::test_command_output(path, expected, |results, spanner, target| {
      test_utils::slice_spans(&results, &spanner, target, direction)
    });
  });
}

/// Checks fixtures against their expected output under incremental compilation,
/// which the IDE uses. Never blesses: the non-incremental tests own the expected files.
fn slice_incremental(dir: &str, filter: impl Fn(&str) -> bool, direction: Direction) {
  test_utils::run_tests_filtered(dir, filter, |path, expected| {
    let Some(expected) = expected else { return };
    let incremental = test_utils::IncrementalDir::new();
    test_utils::test_command_output_with_args(
      path,
      Some(expected),
      &incremental.args(),
      |results, spanner, target| {
        test_utils::slice_spans(&results, &spanner, target, direction)
      },
    );
  });
}

#[test]
fn test_backward_slice() {
  slice("backward_slice", Direction::Backward);
}

#[test]
fn test_forward_slice() {
  slice("forward_slice", Direction::Forward);
}

/// The IDE's focus slice: both directions, with independent call inputs trimmed.
#[test]
fn test_focus_spans() {
  test_utils::run_tests("focus_spans", |path, expected| {
    test_utils::test_command_output(path, expected, |results, spanner, target| {
      let target = spanner
        .span_to_places(target)
        .iter()
        .flat_map(|mir_span| {
          mir_span
            .locations
            .iter()
            .map(|location| (mir_span.place, *location))
        })
        .collect::<Vec<_>>();
      let spans = infoflow::compute_focus_spans(&results, vec![target.clone()], &spanner);
      let mut batch = spanner
        .mir_span_tree
        .iter()
        .map(|span| {
          span
            .locations
            .iter()
            .map(|location| (span.place, *location))
            .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
      batch.push(target);
      let batch = infoflow::compute_focus_spans(&results, batch, &spanner);
      assert_eq!(
        spans[0],
        *batch.last().unwrap(),
        "focus must not depend on batch size"
      );
      Span::merge_overlaps(spans.into_iter().flatten().collect())
    });
  });
}

#[test]
fn test_extensions() {
  slice("extensions", Direction::Backward);
}

#[test]
fn test_extensions_incremental() {
  slice_incremental("extensions", |_| true, Direction::Backward);
}

#[test]
fn test_async_incremental() {
  slice_incremental(
    "backward_slice",
    |name| name.starts_with("async_"),
    Direction::Backward,
  );
}
