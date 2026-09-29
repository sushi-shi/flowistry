//! The evaluation mode passed explicitly to `compute_flow_with_mode` must behave
//! exactly like the same mode set through the ambient `EVAL_MODE`.

#![feature(rustc_private)]

use std::{fs, path::Path};

use flowistry::{
  extensions::EvalMode,
  test_utils::{self, ModeSource},
};
use test_log::test;

fn fixture(name: &str) -> String {
  let path = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join(name);
  fs::read_to_string(path).unwrap()
}

fn slice(input: &str, source: ModeSource) -> Vec<(usize, usize)> {
  test_utils::backward_slice_offsets(input, source, &[])
}

/// For a fixture with a non-default mode header, the explicit and ambient modes agree,
/// and both differ from the default mode (so the mode is actually honored).
fn check_non_default_mode(name: &str) {
  let input = fixture(name);
  let mode = test_utils::eval_mode_from_header(&input);
  assert_ne!(mode, EvalMode::default(), "{name} has no mode header");

  let explicit = slice(&input, ModeSource::Explicit(mode));
  let ambient = slice(&input, ModeSource::Ambient(Some(mode)));
  assert_eq!(
    explicit, ambient,
    "{name}: explicit and ambient mode differ"
  );

  let default = slice(&input, ModeSource::Explicit(EvalMode::default()));
  assert_ne!(explicit, default, "{name}: mode had no effect");
}

#[test]
fn explicit_mode_matches_ambient_ignoremut() {
  check_non_default_mode("extensions/ignoremut_simple.txt");
}

#[test]
fn explicit_mode_matches_ambient_conservative() {
  check_non_default_mode("extensions/conservative_i32_mut_ptr.txt");
}

#[test]
fn explicit_mode_matches_ambient_recurse() {
  check_non_default_mode("extensions/recurse_simple.txt");
}

#[test]
fn unset_ambient_mode_is_default() {
  for name in [
    "extensions/ignoremut_simple.txt",
    "extensions/recurse_simple.txt",
    "backward_slice/function_mut_ptr_param.txt",
  ] {
    let input = fixture(name);
    let unset = slice(&input, ModeSource::Ambient(None));
    let default = slice(&input, ModeSource::Explicit(EvalMode::default()));
    let ambient_default = slice(&input, ModeSource::Ambient(Some(EvalMode::default())));
    assert_eq!(unset, default, "{name}");
    assert_eq!(unset, ambient_default, "{name}");
  }
}
