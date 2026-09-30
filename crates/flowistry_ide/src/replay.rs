//! Answering a request without cargo when nothing that cargo checks has changed.
//!
//! `cargo flowistry` runs `cargo metadata`, then `cargo check`, which makes sure the
//! dependencies of the target crate are up to date and then runs the driver on it.
//! On a typical request, cargo takes 55–75 ms of 180 ms.
//!
//! When the driver runs on the target crate, it records its command line and the
//! variables cargo added to its environment. The record also holds a snapshot of
//! everything that cargo's decisions depend on, taken *before* cargo ran:
//! - manifests, the lockfile, cargo configuration and the toolchain file;
//! - the sources of the other workspace packages;
//! - the inputs of the target package's build script;
//! - the relevant environment variables;
//! - the driver binary.
//!
//! A later request for the same target whose snapshot still matches runs the driver
//! directly. Anything else, including any doubt, takes the cargo path, which then
//! refreshes the record. Set `FLOWISTRY_NO_REPLAY` to always take the cargo path.

use std::{
  collections::{BTreeMap, HashMap},
  env, fs,
  hash::{DefaultHasher, Hash, Hasher},
  io::{BufRead, BufReader, Write},
  path::{Path, PathBuf},
  process::{Command, ExitCode, Stdio},
  time::UNIX_EPOCH,
};

use log::info;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const VERSION: u32 = 1;
const WATCH_VAR: &str = "FLOWISTRY_REPLAY_WATCH";
const DISABLE_VAR: &str = "FLOWISTRY_NO_REPLAY";
const PLUGIN_ARGS: &str = "PLUGIN_ARGS";

/// A file's modification time (in nanoseconds) and length, or `None` if it is missing.
type FileState = Option<(u128, u64)>;

#[derive(Serialize, Deserialize, PartialEq)]
struct Target {
  package: String,
  name: String,
  kind: String,
  src_path: PathBuf,
}

/// Everything cargo's decisions depend on, as it was before cargo ran.
#[derive(Serialize, Deserialize)]
struct Snapshot {
  files: Vec<(PathBuf, FileState)>,
  env: Vec<(String, Option<String>)>,
}

/// Written by `cargo flowistry` for the driver, before running cargo.
#[derive(Serialize, Deserialize)]
struct Pending {
  target: Target,
  targets: Vec<Target>,
  snapshot: Snapshot,
  /// Hashes of the environment of `cargo flowistry`, to tell which variables cargo
  /// added without writing their values to disk.
  env: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize)]
struct Record {
  version: u32,
  target: Target,
  /// Every target of every workspace package, to select the target of a file as
  /// `rustc_plugin` does.
  targets: Vec<Target>,
  snapshot: Snapshot,
  driver: PathBuf,
  cwd: PathBuf,
  args: Vec<String>,
  /// The variables that cargo added to the environment of the driver.
  env: Vec<(String, String)>,
}

fn disabled() -> bool {
  env::var_os(DISABLE_VAR).is_some()
}

fn cache_dir() -> Option<PathBuf> {
  let base = env::var_os("XDG_CACHE_HOME")
    .map(PathBuf::from)
    .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
  Some(base.join("flowistry").join("replay"))
}

/// The record of the target whose sources are in `src_dir`.
fn record_path(src_dir: &Path) -> Option<PathBuf> {
  Some(cache_dir()?.join(format!("{:016x}.json", hash(src_dir))))
}

fn hash(value: impl Hash) -> u64 {
  let mut hasher = DefaultHasher::new();
  value.hash(&mut hasher);
  hasher.finish()
}

/// Writes `contents` to `path` atomically, readable only by the user.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
  use std::os::unix::fs::OpenOptionsExt;
  fs::create_dir_all(path.parent().unwrap())?;
  let temp = path.with_extension(format!("tmp{}", std::process::id()));
  let mut file = fs::OpenOptions::new()
    .write(true)
    .create(true)
    .truncate(true)
    .mode(0o600)
    .open(&temp)?;
  file.write_all(contents)?;
  fs::rename(temp, path)
}

fn file_state(path: &Path) -> FileState {
  let metadata = fs::metadata(path).ok()?;
  let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
  Some((modified.as_nanos(), metadata.len()))
}

/// Adds `dir` and every file and directory under it, except build outputs and hidden
/// directories. A directory's modification time changes when an entry is added or
/// removed.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
  let Ok(entries) = fs::read_dir(dir) else {
    return;
  };
  files.push(dir.to_path_buf());
  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(kind) = entry.file_type() else {
      continue;
    };
    if kind.is_dir() {
      let name = entry.file_name();
      let name = name.to_string_lossy();
      if name != "target" && !name.starts_with('.') {
        walk(&path, files);
      }
    } else {
      files.push(path);
    }
  }
}

/// The target of `file` among `targets`, chosen as `rustc_plugin` chooses it.
fn select<'a>(targets: &'a [Target], file: &Path) -> Option<&'a Target> {
  let mut by_package: BTreeMap<&str, Vec<&Target>> = BTreeMap::new();
  for target in targets {
    if file.starts_with(target.src_path.parent()?) {
      by_package.entry(&target.package).or_default().push(target);
    }
  }
  let stem = file.file_stem()?.to_string_lossy();
  let mut selected = by_package.into_values().filter_map(|matching| {
    if matching.len() == 1 {
      return Some(matching[0]);
    }
    matching
      .iter()
      .find(|target| target.name == stem)
      .or_else(|| {
        let kind = if matching.iter().all(|target| target.kind != "lib") {
          "bin"
        } else if stem == "main" {
          "bin"
        } else {
          "lib"
        };
        matching.iter().find(|target| target.kind == kind)
      })
      .copied()
  });
  let target = selected.next()?;
  selected.next().is_none().then_some(target)
}

/// The inputs of the build script of the package in `package_dir`: the paths and
/// variables it declared (`None` for the paths if it declared none, in which case
/// cargo reruns it when any file of the package changes), or `None` if it has not run.
fn build_script_inputs(
  plugin_dirs: &[PathBuf],
  package: &str,
  package_dir: &Path,
) -> Option<(Option<Vec<PathBuf>>, Vec<String>)> {
  let prefix = format!("{package}-");
  let output = plugin_dirs
    .iter()
    .filter_map(|dir| fs::read_dir(dir.join("debug").join("build")).ok())
    .flatten()
    .flatten()
    .filter(|entry| {
      // `<package>-<16 hex digits>`, not the directory of a package `<package>-…`.
      let name = entry.file_name();
      let hash = name.to_string_lossy();
      let hash = hash.strip_prefix(&prefix).unwrap_or("");
      hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit())
    })
    .map(|entry| entry.path().join("output"))
    .filter(|output| output.exists())
    .max_by_key(|output| file_state(output))?;
  let (paths, vars) = directives(&fs::read_to_string(output).ok()?);
  let paths = (!paths.is_empty())
    .then(|| paths.iter().map(|path| package_dir.join(path)).collect());
  Some((paths, vars))
}

/// The `rerun-if-changed` paths and `rerun-if-env-changed` variables of a build
/// script output.
fn directives(output: &str) -> (Vec<String>, Vec<String>) {
  let (mut paths, mut vars) = (Vec::new(), Vec::new());
  for line in output.lines() {
    let line = line
      .strip_prefix("cargo::")
      .or_else(|| line.strip_prefix("cargo:"));
    if let Some(path) = line.and_then(|line| line.strip_prefix("rerun-if-changed=")) {
      paths.push(path.to_string());
    } else if let Some(var) =
      line.and_then(|line| line.strip_prefix("rerun-if-env-changed="))
    {
      vars.push(var.to_string());
    }
  }
  (paths, vars)
}

fn is_watched_var(name: &str) -> bool {
  // Logging does not change what cargo does.
  if matches!(name, "RUST_LOG" | "RUST_BACKTRACE" | "RUST_LIB_BACKTRACE") {
    return false;
  }
  name.starts_with("CARGO")
    || name.starts_with("RUST")
    || name.starts_with("PKG_CONFIG")
    || matches!(
      name,
      "SYSROOT"
        | "PATH"
        | "LD_LIBRARY_PATH"
        | "CC"
        | "CXX"
        | "CFLAGS"
        | "CXXFLAGS"
        | "LDFLAGS"
    )
}

/// On the cargo path: takes the snapshot and hands it to the driver, which records it
/// if it runs on the target crate. Returns the file to remove once cargo is done.
pub fn prepare(file: &Path) -> Option<PathBuf> {
  if disabled() {
    return None;
  }
  let file = file.canonicalize().ok()?;
  let output = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
    .args(["metadata", "--no-deps", "--all-features", "--offline"])
    .args(["--format-version", "1"])
    .output()
    .ok()?;
  let metadata: Value = serde_json::from_slice(&output.stdout).ok()?;
  let root = PathBuf::from(metadata["workspace_root"].as_str()?);
  let target_dir = PathBuf::from(metadata["target_directory"].as_str()?);

  let mut targets = Vec::new();
  let mut packages = Vec::new();
  for package in metadata["packages"].as_array()? {
    let name = package["name"].as_str()?.to_string();
    let manifest = PathBuf::from(package["manifest_path"].as_str()?);
    let mut build_script = false;
    for target in package["targets"].as_array()? {
      let kind = target["kind"][0].as_str()?.to_string();
      build_script |= kind == "custom-build";
      let src_path = PathBuf::from(target["src_path"].as_str()?);
      targets.push(Target {
        package: name.clone(),
        name: target["name"].as_str()?.to_string(),
        kind,
        src_path: src_path.canonicalize().unwrap_or(src_path),
      });
    }
    packages.push((name, manifest, build_script));
  }
  let target = select(&targets, &file)?;
  // A binary depends on the library of its own package, whose sources are not
  // watched: only libraries, and binaries of packages without one, are replayed.
  let own_lib = targets
    .iter()
    .any(|other| other.package == target.package && other.kind == "lib");
  if target.kind != "lib" && own_lib {
    return None;
  }

  // Path dependencies outside the workspace have no `source` in the lockfile.
  let lock = fs::read_to_string(root.join("Cargo.lock")).ok()?;
  let members = packages
    .iter()
    .map(|(name, ..)| name.as_str())
    .collect::<Vec<_>>();
  let mut outside = false;
  for entry in lock.split("[[package]]").skip(1) {
    let name = entry
      .lines()
      .find_map(|line| line.strip_prefix("name = "))
      .map(|name| name.trim_matches('"'));
    if !entry.contains("\nsource = ") && name.is_some_and(|name| !members.contains(&name))
    {
      outside = true;
    }
  }
  if outside {
    return None;
  }

  let plugin_dirs = fs::read_dir(&target_dir)
    .ok()?
    .flatten()
    .filter(|entry| entry.file_name().to_string_lossy().starts_with("plugin-"))
    .map(|entry| entry.path())
    .collect::<Vec<_>>();

  let mut files = vec![root.join("Cargo.lock"), root.join("Cargo.toml")];
  files.extend(["rust-toolchain", "rust-toolchain.toml"].map(|name| root.join(name)));
  for dir in root.ancestors() {
    files.extend(["config", "config.toml"].map(|name| dir.join(".cargo").join(name)));
  }
  let cargo_home = env::var_os("CARGO_HOME")
    .map(PathBuf::from)
    .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
  if let Some(home) = cargo_home {
    files.extend(["config", "config.toml"].map(|name| home.join(name)));
  }
  files.push(env::current_exe().ok()?.with_file_name("flowistry-driver"));
  let mut vars = Vec::new();
  for (name, manifest, build_script) in &packages {
    files.push(manifest.clone());
    let package_dir = manifest.parent()?;
    if *name != target.package {
      walk(package_dir, &mut files);
    }
    if *build_script {
      let (paths, declared) = build_script_inputs(&plugin_dirs, name, package_dir)?;
      vars.extend(declared);
      match paths {
        Some(paths) => {
          for path in paths {
            if path.is_dir() {
              walk(&path, &mut files);
            } else {
              files.push(path);
            }
          }
        }
        None => walk(package_dir, &mut files),
      }
    }
  }
  // Build scripts of dependencies read the variables they declared.
  for dir in &plugin_dirs {
    for entry in fs::read_dir(dir.join("debug").join("build"))
      .into_iter()
      .flatten()
      .flatten()
    {
      if let Ok(output) = fs::read_to_string(entry.path().join("output")) {
        vars.extend(directives(&output).1);
      }
    }
  }
  files.sort();
  files.dedup();

  let env_now = env::vars()
    .map(|(name, value)| {
      let value = hash(&value);
      (name, value)
    })
    .collect::<BTreeMap<_, _>>();
  vars.extend(env_now.keys().filter(|name| is_watched_var(name)).cloned());
  vars.sort();
  vars.dedup();
  let snapshot = Snapshot {
    files: files
      .into_iter()
      .map(|path| {
        let state = file_state(&path);
        (path, state)
      })
      .collect(),
    env: vars
      .into_iter()
      .map(|name| {
        let value = env::var(&name).ok();
        (name, value)
      })
      .collect(),
  };

  let pending = Pending {
    target: Target {
      package: target.package.clone(),
      name: target.name.clone(),
      kind: target.kind.clone(),
      src_path: target.src_path.clone(),
    },
    targets,
    snapshot,
    env: env_now,
  };
  let path = cache_dir()?.join(format!("pending-{}.json", std::process::id()));
  write_private(&path, &serde_json::to_vec(&pending).ok()?).ok()?;
  // SAFETY: `cargo flowistry` is single-threaded here, before cargo runs.
  unsafe { env::set_var(WATCH_VAR, &path) };
  Some(path)
}

/// In the driver, on the target crate: records how to run it again.
pub fn record(args: &[String]) {
  let Some(path) = env::var_os(WATCH_VAR) else {
    return;
  };
  let Some(pending) = fs::read(&path)
    .ok()
    .and_then(|bytes| serde_json::from_slice::<Pending>(&bytes).ok())
  else {
    return;
  };
  let env = env::vars()
    .filter(|(name, value)| {
      // The jobserver of this cargo run is gone by the time of a replay.
      !matches!(
        name.as_str(),
        WATCH_VAR | PLUGIN_ARGS | "CARGO_MAKEFLAGS" | "MAKEFLAGS"
      ) && pending.env.get(name) != Some(&hash(value))
    })
    .collect();
  let (Ok(driver), Ok(cwd)) = (env::current_exe(), env::current_dir()) else {
    return;
  };
  let record = Record {
    version: VERSION,
    target: pending.target,
    targets: pending.targets,
    snapshot: pending.snapshot,
    driver,
    cwd,
    args: args.to_vec(),
    env,
  };
  let path = record.target.src_path.parent().and_then(record_path);
  if let (Some(path), Ok(bytes)) = (path, serde_json::to_vec(&record)) {
    let _ = write_private(&path, &bytes);
  }
}

/// Why `snapshot` no longer holds, if it does not.
fn stale(snapshot: &Snapshot) -> Option<String> {
  for (path, state) in &snapshot.files {
    if file_state(path) != *state {
      return Some(format!("{} changed", path.display()));
    }
  }
  let env = env::vars().collect::<HashMap<_, _>>();
  for (name, value) in &snapshot.env {
    if env.get(name) != value.as_ref() {
      return Some(format!("${name} changed"));
    }
  }
  if env.keys().any(|name| {
    is_watched_var(name) && !snapshot.env.iter().any(|(watched, _)| watched == name)
  }) {
    return Some("a new variable is set".into());
  }
  None
}

/// Runs the driver directly if a record for the target of `file` still holds.
pub fn try_replay(file: &Path, plugin_args: &str) -> Option<ExitCode> {
  if disabled() {
    return None;
  }
  let file = file.canonicalize().ok()?;
  let record = file
    .ancestors()
    .skip(1)
    .filter_map(|dir| fs::read(record_path(dir)?).ok())
    .filter_map(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
    .find(|record| {
      record.version == VERSION && select(&record.targets, &file) == Some(&record.target)
    })?;
  if let Some(reason) = stale(&record.snapshot) {
    info!("replay: {reason}, running cargo");
    return None;
  }
  info!("replay: running the driver directly");
  // The arguments start with the driver's own path; cargo runs it as a wrapper of
  // `rustc`.
  let mut driver = Command::new(&record.driver)
    .arg("rustc")
    .args(record.args.iter().skip(1))
    .current_dir(&record.cwd)
    .envs(record.env.iter().map(|(name, value)| (name, value)))
    .env(PLUGIN_ARGS, plugin_args)
    .env_remove(WATCH_VAR)
    .stderr(Stdio::piped())
    .spawn()
    .ok()?;
  // rustc writes its messages as JSON for cargo: print diagnostics as cargo does, and
  // leave out the other messages.
  let mut stderr = std::io::stderr().lock();
  for line in BufReader::new(driver.stderr.take()?).lines() {
    let Ok(line) = line else { break };
    match serde_json::from_str::<Value>(&line) {
      Ok(message) if message["$message_type"].is_string() => {
        if let Some(rendered) = message["rendered"].as_str() {
          let _ = stderr.write_all(rendered.as_bytes());
        }
      }
      _ => {
        let _ = writeln!(stderr, "{line}");
      }
    }
  }
  let status = driver.wait().ok()?;
  Some(ExitCode::from(status.code().unwrap_or(1) as u8))
}
