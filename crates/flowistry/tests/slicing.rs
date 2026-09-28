#![feature(rustc_private)]

use flowistry::{infoflow::Direction, test_utils};
use test_log::test;

fn slice(dir: &str, direction: Direction) {
  test_utils::run_tests(dir, |path, expected| {
    test_utils::test_command_output(path, expected, |results, spanner, target| {
      test_utils::slice_spans(&results, &spanner, target, direction)
    });
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

#[test]
fn test_extensions() {
  slice("extensions", Direction::Backward);
}
