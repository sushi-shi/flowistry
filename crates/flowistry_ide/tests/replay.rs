//! End-to-end tests of running the driver without cargo (see `replay.rs`): on a
//! workspace whose `app` library depends on the `dep` library and has a build script.

use std::{
  fs,
  io::Read,
  path::{Path, PathBuf},
  process::Command,
};

use base64::Engine;
use serde_json::Value;

const APP: &str = "pub fn handle(input: i32) -> i32 {\n    let doubled = dep::double(input);\n    doubled + 1\n}\n";

/// A fresh workspace and cache directory for one test.
fn setup(name: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join("replay")
    .join(name);
  let _ = fs::remove_dir_all(&dir);
  let write = |path: &str, contents: &str| {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
  };
  write(
    "Cargo.toml",
    "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n",
  );
  write(
    "dep/Cargo.toml",
    "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
  );
  write(
    "dep/src/lib.rs",
    "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n",
  );
  write(
    "app/Cargo.toml",
    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
  );
  write(
    "app/build.rs",
    "fn main() {\n    println!(\"cargo:rerun-if-changed=data.txt\");\n}\n",
  );
  write("app/data.txt", "1\n");
  write("app/src/lib.rs", APP);
  dir
}

struct Run {
  places: Option<usize>,
  replayed: bool,
  stderr: String,
}

fn focus(dir: &Path, env: &[(&str, &str)]) -> Run {
  let binary = Path::new(env!("CARGO_BIN_EXE_cargo-flowistry"));
  let path = format!(
    "{}:{}",
    binary.parent().unwrap().display(),
    std::env::var("PATH").unwrap_or_default()
  );
  let result = Command::new(binary)
    .current_dir(dir)
    .env("PATH", path)
    .env("XDG_CACHE_HOME", dir.join("cache"))
    .env("RUST_LOG", "flowistry_ide::replay=info")
    .env_remove("FLOWISTRY_NO_REPLAY")
    // Each workspace builds into its own target directory.
    .env_remove("CARGO_TARGET_DIR")
    .envs(env.iter().copied())
    .args(["flowistry", "focus"])
    .arg(dir.join("app/src/lib.rs"))
    .args(["1", "8"])
    .output()
    .unwrap();
  let stdout = String::from_utf8(result.stdout).unwrap();
  let places = stdout.trim().lines().last().map(|encoded| {
    let compressed = base64::engine::general_purpose::STANDARD
      .decode(encoded)
      .unwrap();
    let mut json = String::new();
    flate2::read::GzDecoder::new(&compressed[..])
      .read_to_string(&mut json)
      .unwrap();
    let output: Value = serde_json::from_str(&json).unwrap();
    output["Ok"]["place_info"].as_array().unwrap().len()
  });
  let stderr = String::from_utf8(result.stderr).unwrap();
  Run {
    places,
    replayed: stderr.contains("replay: running the driver directly"),
    stderr,
  }
}

/// The first request runs cargo before the build script has run, so its inputs are
/// unknown and it records nothing; the second request records.
fn warm_up(dir: &Path) -> Run {
  focus(dir, &[]);
  let run = focus(dir, &[]);
  assert!(!run.replayed && run.places.is_some(), "{}", run.stderr);
  run
}

fn edit(dir: &Path, path: &str, contents: &str) {
  fs::write(dir.join(path), contents).unwrap();
}

#[test]
fn replays_when_nothing_changed() {
  let dir = setup("unchanged");
  let first = warm_up(&dir);
  let second = focus(&dir, &[]);
  assert!(second.replayed, "{}", second.stderr);
  assert_eq!(second.places, first.places);
}

#[test]
fn edits_to_the_target_crate_replay_with_the_new_code() {
  let dir = setup("target_edit");
  let first = warm_up(&dir);
  edit(
    &dir,
    "app/src/lib.rs",
    &APP.replace(
      "    doubled + 1\n",
      "    let tripled = doubled * 3;\n    tripled + 1\n",
    ),
  );
  let second = focus(&dir, &[]);
  assert!(second.replayed, "{}", second.stderr);
  assert!(
    second.places > first.places,
    "{:?} {:?}",
    first.places,
    second.places
  );
}

#[test]
fn changes_to_a_dependency_run_cargo() {
  let dir = setup("dependency");
  warm_up(&dir);
  // With the old metadata of `dep`, the new call would not type-check.
  edit(
    &dir,
    "dep/src/lib.rs",
    "pub fn double(x: i32, y: i32) -> i32 {\n    x * y\n}\n",
  );
  edit(
    &dir,
    "app/src/lib.rs",
    &APP.replace("double(input)", "double(input, 2)"),
  );
  let run = focus(&dir, &[]);
  assert!(!run.replayed, "{}", run.stderr);
  assert!(run.places.is_some(), "{}", run.stderr);
  // And the next request replays again.
  assert!(focus(&dir, &[]).replayed);
}

#[test]
fn changes_to_a_manifest_run_cargo() {
  let dir = setup("manifest");
  warm_up(&dir);
  let manifest = fs::read_to_string(dir.join("app/Cargo.toml")).unwrap();
  edit(
    &dir,
    "app/Cargo.toml",
    &format!("{manifest}\n[features]\nextra = []\n"),
  );
  assert!(!focus(&dir, &[]).replayed);
}

#[test]
fn changes_to_build_script_inputs_run_cargo() {
  let dir = setup("build_script");
  warm_up(&dir);
  edit(&dir, "app/data.txt", "2\n");
  assert!(!focus(&dir, &[]).replayed);
}

#[test]
fn changes_to_the_environment_run_cargo() {
  let dir = setup("environment");
  warm_up(&dir);
  let run = focus(&dir, &[("RUSTFLAGS", "--cfg flowistry_test")]);
  assert!(!run.replayed, "{}", run.stderr);
}

#[test]
fn replayed_errors_are_rendered_like_cargo() {
  let dir = setup("errors");
  warm_up(&dir);
  edit(
    &dir,
    "app/src/lib.rs",
    &APP.replace(
      "let doubled = dep::double(input)",
      "let doubled: i32 = \"no\"",
    ),
  );
  let run = focus(&dir, &[]);
  assert!(run.replayed, "{}", run.stderr);
  assert!(run.places.is_none());
  assert!(run.stderr.contains("error[E0308]"), "{}", run.stderr);
  assert!(!run.stderr.contains("$message_type"), "{}", run.stderr);
}

#[test]
fn replay_can_be_disabled() {
  let dir = setup("disabled");
  warm_up(&dir);
  assert!(!focus(&dir, &[("FLOWISTRY_NO_REPLAY", "1")]).replayed);
}

#[test]
fn new_files_in_a_dependency_run_cargo() {
  let dir = setup("new_file");
  warm_up(&dir);
  edit(&dir, "dep/src/extra.rs", "pub fn extra() {}\n");
  assert!(!focus(&dir, &[]).replayed);
}
