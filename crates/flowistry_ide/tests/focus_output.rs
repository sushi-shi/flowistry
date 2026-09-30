//! End-to-end checks of the output of `cargo flowistry focus` on a small generated crate.

use std::{collections::HashSet, fs, io::Read, path::Path, process::Command};

use base64::Engine;
use serde_json::Value;

/// `x` and `y` are each mentioned at several locations and flow into each other.
const LIB: &str = r#"pub fn f(mut x: i32) -> i32 {
    let y = x + 1;
    x = y * 2;
    let z = x + y + x;
    z + y
}
"#;

fn focus(line: usize, column: usize) -> Value {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("focus_output");
  fs::create_dir_all(dir.join("src")).unwrap();
  fs::write(
    dir.join("Cargo.toml"),
    "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
  )
  .unwrap();
  fs::write(dir.join("src/lib.rs"), LIB).unwrap();

  let binary = Path::new(env!("CARGO_BIN_EXE_cargo-flowistry"));
  let path = format!(
    "{}:{}",
    binary.parent().unwrap().display(),
    std::env::var("PATH").unwrap_or_default()
  );
  let output = Command::new(binary)
    .current_dir(&dir)
    .env("PATH", path)
    .args(["flowistry", "focus", "src/lib.rs"])
    .args([line.to_string(), column.to_string()])
    .output()
    .unwrap();
  let stdout = String::from_utf8(output.stdout).unwrap();
  let encoded = stdout
    .trim()
    .lines()
    .last()
    .unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(&output.stderr)));
  let compressed = base64::engine::general_purpose::STANDARD
    .decode(encoded)
    .unwrap();
  let mut json = String::new();
  flate2::read::GzDecoder::new(&compressed[..])
    .read_to_string(&mut json)
    .unwrap();
  serde_json::from_str(&json).unwrap()
}

#[test]
fn range_lists_have_no_repetitions() {
  let output = focus(1, 8);
  let places = output["Ok"]["place_info"].as_array().unwrap();
  assert!(!places.is_empty());
  let mut influenced = 0;
  for place in places {
    for field in ["ranges", "slice", "direct_influence"] {
      let ranges = place[field].as_array().unwrap();
      let distinct = ranges.iter().map(Value::to_string).collect::<HashSet<_>>();
      assert_eq!(
        distinct.len(),
        ranges.len(),
        "{field} of {}",
        place["range"]
      );
    }
    influenced += usize::from(!place["direct_influence"].as_array().unwrap().is_empty());
  }
  // The places are mentioned at several locations, which used to repeat their spans.
  assert!(influenced > 0);
}
