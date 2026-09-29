//! End-to-end tests of `cargo flowistry focus` on generated crates: the analysis runs
//! right after expansion, so rustc only checks what it needs.

use std::{
  fs,
  io::Read,
  path::{Path, PathBuf},
  process::Command,
};

use base64::Engine;
use serde_json::Value;

const LIB: &str = r#"fn scale(value: i32, factor: i32) -> i32 {
    value * factor
}

pub fn handle(input: i32) -> i32 {
    let doubled = input * 2;
    scale(doubled, 3)
}

pub fn a() -> i32 { 1 }
pub fn b() -> i32 { 2 }
pub fn c() -> i32 { 3 }
"#;

/// Writes a crate whose `src/lib.rs` is `LIB` followed by `extra`.
fn setup(name: &str, extra: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join("demand_driven")
    .join(name);
  fs::create_dir_all(dir.join("src")).unwrap();
  fs::write(
    dir.join("Cargo.toml"),
    "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
  )
  .unwrap();
  fs::write(dir.join("src/lib.rs"), format!("{LIB}{extra}")).unwrap();
  dir
}

struct Run {
  output: Option<Value>,
  stderr: String,
}

fn focus(dir: &Path, mode: &str, line: usize, column: usize) -> Run {
  let binary = Path::new(env!("CARGO_BIN_EXE_cargo-flowistry"));
  let path = format!(
    "{}:{}",
    binary.parent().unwrap().display(),
    std::env::var("PATH").unwrap_or_default()
  );
  let result = Command::new(binary)
    .current_dir(dir)
    .env("PATH", path)
    .env("RUST_LOG", "rustc_utils::timer=info")
    .args(["flowistry", "--context-mode", mode, "focus", "src/lib.rs"])
    .args([line.to_string(), column.to_string()])
    .output()
    .unwrap();
  let stdout = String::from_utf8(result.stdout).unwrap();
  let output = stdout.trim().lines().last().map(|encoded| {
    let compressed = base64::engine::general_purpose::STANDARD
      .decode(encoded)
      .unwrap();
    let mut json = String::new();
    flate2::read::GzDecoder::new(&compressed[..])
      .read_to_string(&mut json)
      .unwrap();
    serde_json::from_str(&json).unwrap()
  });
  Run {
    output,
    stderr: String::from_utf8(result.stderr).unwrap(),
  }
}

/// The bodies whose borrowck facts were collected, from rustc_utils' timer log.
fn bodies_with_facts(stderr: &str) -> Vec<&str> {
  stderr
    .lines()
    .filter_map(|line| line.split("get_bodies_with_borrowck_facts for ").nth(1))
    .collect()
}

fn places(run: &Run) -> usize {
  let output = run
    .output
    .as_ref()
    .unwrap_or_else(|| panic!("{}", run.stderr));
  output["Ok"]["place_info"].as_array().unwrap().len()
}

#[test]
fn only_the_focused_body_is_borrow_checked() {
  let dir = setup("focused_body", "");
  // `let doubled` in `handle`.
  let run = focus(&dir, "SigOnly", 5, 8);
  assert!(places(&run) > 0);
  let bodies = bodies_with_facts(&run.stderr);
  assert_eq!(bodies.len(), 1, "{bodies:?}");
  assert!(bodies[0].contains("handle"), "{bodies:?}");
}

#[test]
fn recurse_mode_checks_the_callees_it_needs() {
  let dir = setup("recurse", "");
  let run = focus(&dir, "Recurse", 5, 8);
  assert!(places(&run) > 0);
  let bodies = bodies_with_facts(&run.stderr);
  assert!(
    bodies.iter().any(|body| body.contains("handle")),
    "{bodies:?}"
  );
  assert!(
    bodies.iter().any(|body| body.contains("scale")),
    "{bodies:?}"
  );
  assert!(
    !bodies.iter().any(|body| body.contains("::a took")),
    "{bodies:?}"
  );
}

#[test]
fn errors_elsewhere_do_not_block_the_focus() {
  let dir = setup(
    "error_elsewhere",
    "\npub fn broken() -> i32 {\n    \"not a number\"\n}\n",
  );
  let run = focus(&dir, "SigOnly", 5, 8);
  assert!(places(&run) > 0);
}

#[test]
fn errors_in_the_focused_body_fail_the_request() {
  let dir = setup(
    "error_in_target",
    "\npub fn broken() -> i32 {\n    let x: i32 = \"no\";\n    x\n}\n",
  );
  let run = focus(&dir, "SigOnly", 14, 8);
  assert!(run.output.is_none(), "{:?}", run.output);
  assert!(run.stderr.contains("error[E0308]"), "{}", run.stderr);
  assert!(
    !run.stderr.contains("internal compiler error"),
    "{}",
    run.stderr
  );
}
