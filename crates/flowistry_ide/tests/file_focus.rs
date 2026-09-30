//! End-to-end tests of `cargo flowistry file-focus` on a small generated crate.

use std::{
  fs,
  io::Read,
  path::{Path, PathBuf},
  process::Command,
};

use base64::Engine;
use serde_json::Value;

const LIB: &str = r#"mod other;

fn scale(value: i32, factor: i32) -> i32 {
    value * factor
}

pub fn handle(input: i32, factor: i32) -> i32 {
    let doubled = input * 2;
    let add = |x: i32| x + doubled;
    scale(add(doubled), factor)
}
"#;

const OTHER: &str = r#"pub fn other(x: i32) -> i32 {
    x + 1
}
"#;

/// Writes the crate into a directory of its own, so tests can run in parallel.
fn setup(name: &str, other: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join("file_focus")
    .join(name);
  fs::create_dir_all(dir.join("src")).unwrap();
  fs::write(
    dir.join("Cargo.toml"),
    "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
  )
  .unwrap();
  fs::write(dir.join("src/lib.rs"), LIB).unwrap();
  fs::write(dir.join("src/other.rs"), other).unwrap();
  dir
}

/// Runs `file-focus` and returns the decoded output, or its stderr if there is none
/// (a build error prints no output, though the command still exits successfully).
fn file_focus(
  dir: &Path,
  file: &str,
  position: Option<(usize, usize)>,
) -> Result<Value, String> {
  let binary = Path::new(env!("CARGO_BIN_EXE_cargo-flowistry"));
  let path = format!(
    "{}:{}",
    binary.parent().unwrap().display(),
    std::env::var("PATH").unwrap_or_default()
  );
  let mut command = Command::new(binary);
  command
    .current_dir(dir)
    .env("PATH", path)
    .args(["flowistry", "file-focus"])
    .arg(dir.join(file));
  if let Some((line, column)) = position {
    command.args([line.to_string(), column.to_string()]);
  }
  let output = command.output().unwrap();
  let stdout = String::from_utf8(output.stdout).unwrap();
  let Some(encoded) = stdout.trim().lines().last() else {
    return Err(String::from_utf8(output.stderr).unwrap());
  };
  let compressed = base64::engine::general_purpose::STANDARD
    .decode(encoded)
    .unwrap();
  let mut json = String::new();
  flate2::read::GzDecoder::new(&compressed[..])
    .read_to_string(&mut json)
    .unwrap();
  Ok(serde_json::from_str(&json).unwrap())
}

/// Each body as `(first line, last line, analyzed)`, with 0-based lines, sorted.
fn bodies(output: &Value) -> Vec<(u64, u64, bool)> {
  let mut bodies = output["Ok"]["bodies"]
    .as_array()
    .unwrap()
    .iter()
    .map(|body| {
      let focus = &body["focus"];
      assert!(focus.is_null() || focus.get("Ok").is_some(), "{focus}");
      (
        body["range"]["start"]["line"].as_u64().unwrap(),
        body["range"]["end"]["line"].as_u64().unwrap(),
        !focus.is_null(),
      )
    })
    .collect::<Vec<_>>();
  bodies.sort();
  bodies
}

#[test]
fn whole_file_analyzes_every_body_of_that_file_only() {
  let dir = setup("whole_file", OTHER);
  let output = file_focus(&dir, "src/lib.rs", None).unwrap();
  // `scale`, `handle` and the closure; nothing from `other.rs`.
  assert_eq!(bodies(&output), vec![
    (2, 4, true),
    (6, 10, true),
    (8, 8, true)
  ]);
}

#[test]
fn position_analyzes_only_the_innermost_body() {
  let dir = setup("position", OTHER);

  // `let doubled` in `handle`.
  let output = file_focus(&dir, "src/lib.rs", Some((7, 8))).unwrap();
  assert_eq!(bodies(&output), vec![
    (2, 4, false),
    (6, 10, true),
    (8, 8, false)
  ]);

  // `x + doubled`, inside the closure, which is nested in `handle`.
  let output = file_focus(&dir, "src/lib.rs", Some((8, 23))).unwrap();
  assert_eq!(bodies(&output), vec![
    (2, 4, false),
    (6, 10, false),
    (8, 8, true)
  ]);
}

#[test]
fn module_file() {
  let dir = setup("module_file", OTHER);
  let output = file_focus(&dir, "src/other.rs", Some((1, 4))).unwrap();
  assert_eq!(bodies(&output), vec![(0, 2, true)]);
}

/// Borrowck facts are only collected for the requested bodies, but rustc still
/// borrow-checks the others: an error there fails the request as before.
#[test]
fn borrowck_errors_outside_the_scope_are_still_reported() {
  let dir = setup(
    "borrowck_error",
    "pub fn other() -> usize {\n    let v = vec![1];\n    let r = &v;\n    drop(v);\n    r.len()\n}\n",
  );
  for position in [Some((7, 8)), None] {
    let stderr = file_focus(&dir, "src/lib.rs", position).unwrap_err();
    assert!(stderr.contains("error[E0505]"), "{stderr}");
  }
}
