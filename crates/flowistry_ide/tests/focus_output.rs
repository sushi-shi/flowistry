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
  focus_source("focus_output", LIB, line, column)
}

/// Focuses at `line` and `column` in a crate whose `src/lib.rs` is `source`.
fn focus_source(name: &str, source: &str, line: usize, column: usize) -> Value {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
  fs::create_dir_all(dir.join("src")).unwrap();
  fs::write(
    dir.join("Cargo.toml"),
    "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
  )
  .unwrap();
  fs::write(dir.join("src/lib.rs"), source).unwrap();

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
    for field in [
      "ranges",
      "slice",
      "pre_slice",
      "post_slice",
      "direct_influence",
    ] {
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

#[test]
fn places_refer_to_a_table_of_distinct_ranges() {
  let output = focus(1, 8);
  let table = output["Ok"]["ranges"].as_array().unwrap();
  let distinct = table.iter().map(Value::to_string).collect::<HashSet<_>>();
  assert_eq!(distinct.len(), table.len(), "the table repeats a range");

  let mut used = HashSet::new();
  for place in output["Ok"]["place_info"].as_array().unwrap() {
    let range = place["range"].as_u64().unwrap();
    let lists = [
      "ranges",
      "slice",
      "pre_slice",
      "post_slice",
      "direct_influence",
    ]
    .into_iter()
    .flat_map(|field| place[field].as_array().unwrap());
    for index in lists.map(|index| index.as_u64().unwrap()).chain([range]) {
      assert!(
        (index as usize) < table.len(),
        "index {index} out of the table"
      );
      used.insert(index);
    }
  }
  for index in output["Ok"]["comments"]
    .as_array()
    .into_iter()
    .flatten()
    .chain(
      output["Ok"]["parameter_aliases"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|alias| [&alias["range"], &alias["target"]]),
    )
  {
    let index = index.as_u64().unwrap();
    assert!((index as usize) < table.len());
    used.insert(index);
  }
  assert_eq!(used.len(), table.len(), "the table has unused ranges");
}

/// Ranges count characters, not bytes: the two non-ASCII characters before `x` take
/// 6 bytes but 2 columns.
#[test]
fn ranges_are_in_characters() {
  let source = "pub fn f() -> usize {\n    let s = \"🦀é\"; let x = s.len();\n    x\n}\n";
  let output = focus_source("characters", source, 1, 22);
  let table = output["Ok"]["ranges"].as_array().unwrap();
  let x = output["Ok"]["place_info"]
    .as_array()
    .unwrap()
    .iter()
    .map(|place| &table[place["range"].as_u64().unwrap() as usize])
    .find(|range| range["start"]["line"] == 1 && range["start"]["column"] == 22)
    .unwrap_or_else(|| panic!("no place at 1:22 in {output}"));
  assert_eq!(x["end"]["column"], 23, "{x}");
}
