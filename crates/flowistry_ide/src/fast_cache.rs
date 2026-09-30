//! Replay successful file-focus responses before Cargo/rustc startup.
//!
//! The compiler-validated cache remains responsible for semantic reuse after
//! edits. This layer only accepts an unchanged Cargo input snapshot.
use std::{
  collections::{BTreeMap, BTreeSet},
  env, fs,
  hash::{Hash, Hasher},
  io::{Read, Write},
  path::{Path, PathBuf},
  process::{Command, ExitCode, Stdio},
  time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use rustc_data_structures::{fingerprint::Fingerprint, stable_hasher::StableHasher};
use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CHILD: &str = "FLOWISTRY_CACHE_CHILD_INPUTS";
const LIMIT: u64 = 32 * 1024 * 1024;

// These describe the launcher, rather than the build. Nix changes its scratch
// directory on every invocation. Explicit use in Rust/build-script dep-info
// disables replay, so excluding them cannot hide declared semantic inputs.
fn transient_env(name: &str) -> bool {
  matches!(
    name,
    "NIX_BUILD_TOP"
      | "TMPDIR"
      | "TMP"
      | "TEMP"
      | "TEMPDIR"
      | "SHLVL"
      | "_"
      | "FLOWISTRY_CACHE"
      | CHILD
  )
}

fn digest(value: &impl Hash) -> String {
  let mut h = StableHasher::new();
  value.hash(&mut h);
  let value: Fingerprint = h.finish();
  format!("{value:?}")
}

fn file_digest(path: &Path) -> Option<String> {
  let mut file = fs::File::open(path).ok()?;
  let mut h = StableHasher::new();
  let mut buffer = [0; 64 * 1024];
  loop {
    let count = file.read(&mut buffer).ok()?;
    if count == 0 {
      break;
    }
    h.write(&buffer[.. count]);
  }
  let value: Fingerprint = h.finish();
  Some(format!("{value:?}"))
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Stamp {
  len: u64,
  modified: u128,
  // ctime detects same-size writes with a restored mtime on Unix.
  changed: (i64, i64),
  inode: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Input {
  stamp: Stamp,
  digest: String,
}
type Snapshot = BTreeMap<PathBuf, Option<Input>>;

fn same_contents(a: &Snapshot, b: &Snapshot) -> bool {
  a.len() == b.len()
    && a.iter().all(|(path, input)| {
      b.get(path).is_some_and(|other| {
        input.as_ref().map(|i| &i.digest) == other.as_ref().map(|i| &i.digest)
      })
    })
}
fn stamp(path: &Path) -> Option<Stamp> {
  let m = fs::metadata(path).ok()?;
  #[cfg(unix)]
  let (changed, inode) = {
    use std::os::unix::fs::MetadataExt;
    ((m.ctime(), m.ctime_nsec()), m.ino())
  };
  #[cfg(not(unix))]
  let (changed, inode) = return None; // No trustworthy change stamp: use compiler validation.
  Some(Stamp {
    len: m.len(),
    modified: m
      .modified()
      .ok()?
      .duration_since(UNIX_EPOCH)
      .ok()?
      .as_nanos(),
    changed,
    inode,
  })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
  key: String,
  workspace: PathBuf,
  roots: Vec<PathBuf>,
  excluded: Vec<PathBuf>,
  files: Vec<PathBuf>,
  snapshot: Snapshot,
  responses: Vec<Response>,
  checksum: String,
}
impl Entry {
  fn checksum(&self) -> Option<String> {
    Some(digest(
      &serde_json::to_vec(&(
        &self.key,
        &self.workspace,
        &self.roots,
        &self.excluded,
        &self.files,
        &self.snapshot,
        &self.responses,
      ))
      .ok()?,
    ))
  }
  fn snapshot(&self) -> Option<Snapshot> {
    fn walk(
      path: &Path,
      excluded: &[PathBuf],
      result: &mut BTreeMap<PathBuf, Option<Stamp>>,
      seen: &mut BTreeSet<PathBuf>,
    ) -> Option<()> {
      if excluded.iter().any(|p| path.starts_with(p)) {
        return Some(());
      }
      if result.len() > 100_000 {
        return None;
      }
      let metadata = fs::metadata(path).ok()?;
      if metadata.is_dir() {
        // Resolve symlinks, including links out of a package, and reject cycles.
        let real = path.canonicalize().ok()?;
        if !seen.insert(real) {
          return None;
        }
        for child in fs::read_dir(path).ok()? {
          let child = child.ok()?.path();
          if child.file_name().is_some_and(|n| n == ".git") {
            continue;
          }
          walk(&child, excluded, result, seen)?;
        }
        seen.remove(&path.canonicalize().ok()?);
      } else if metadata.is_file() {
        result.insert(path.to_owned(), Some(stamp(path)?));
      } else {
        return None;
      }
      Some(())
    }
    let mut result = BTreeMap::new();
    for root in &self.roots {
      walk(root, &self.excluded, &mut result, &mut BTreeSet::new())?;
    }
    for file in &self.files {
      result.insert(
        file.clone(),
        if file.exists() {
          Some(stamp(file)?)
        } else {
          None
        },
      );
    }
    result
      .into_iter()
      .map(|(path, current)| {
        let Some(current) = current else {
          return Some((path, None));
        };
        let old = self.snapshot.get(&path).and_then(Option::as_ref);
        let hash = if let Some(old) = old.filter(|old| old.stamp == current) {
          old.digest.clone()
        } else if path.is_dir() {
          let mut children = fs::read_dir(&path)
            .ok()?
            .collect::<std::io::Result<Vec<_>>>()
            .ok()?
            .into_iter()
            .map(|e| e.path())
            .filter(|p| {
              !self
                .excluded
                .iter()
                .any(|e| !path.starts_with(e) && p.starts_with(e))
                && p.file_name().is_none_or(|n| n != ".git")
            })
            .collect::<Vec<_>>();
          children.sort();
          digest(&children)
        } else {
          file_digest(&path)?
        };
        Some((
          path,
          Some(Input {
            stamp: current,
            digest: hash,
          }),
        ))
      })
      .collect()
  }
}

fn directory() -> Option<PathBuf> {
  env::var_os("FLOWISTRY_CACHE_DIR")
    .map(PathBuf::from)
    .or_else(|| {
      env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .map(|p| p.join("flowistry"))
    })
}

fn program(name: &str) -> Option<PathBuf> {
  env::split_paths(&env::var_os("PATH")?)
    .map(|p| p.join(name))
    .find(|p| p.is_file())
}

fn discover(key: String, directory: &Path, selected_file: &Path) -> Option<Entry> {
  let output = Command::new("cargo")
    .args([
      "metadata",
      "--offline",
      "--all-features",
      "--format-version",
      "1",
    ])
    .output()
    .ok()?;
  if !output.status.success() {
    return None;
  }
  let metadata: Value = serde_json::from_slice(&output.stdout).ok()?;
  let workspace = PathBuf::from(metadata["workspace_root"].as_str()?);
  let target = PathBuf::from(metadata["target_directory"].as_str()?);
  let mut roots = BTreeSet::new();
  let mut files = BTreeSet::new();
  let packages = metadata["packages"].as_array()?;
  let selected_file = selected_file.canonicalize().ok()?;
  let selected = packages
    .iter()
    .filter(|p| p["source"].is_null())
    .filter_map(|p| {
      let manifest = Path::new(p["manifest_path"].as_str()?);
      let root = manifest.parent()?;
      selected_file
        .starts_with(root)
        .then_some((root.components().count(), p["id"].as_str()?))
    })
    .max_by_key(|(depth, _)| *depth)?
    .1;
  let nodes = metadata["resolve"]["nodes"].as_array()?;
  let mut reachable = BTreeSet::new();
  let mut pending = vec![selected];
  while let Some(id) = pending.pop() {
    if !reachable.insert(id) {
      continue;
    }
    let node = nodes.iter().find(|n| n["id"].as_str() == Some(id))?;
    pending.extend(
      node["dependencies"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str),
    );
  }
  // Registry/git sources are also watched: changing vendored dependencies must
  // not be hidden by a warm response cache.
  for package in packages {
    let manifest = PathBuf::from(package["manifest_path"].as_str()?);
    files.insert(manifest.clone());
    // Discovering a new member of a workspace glob can change feature
    // unification without editing an existing manifest.
    for parent in manifest
      .ancestors()
      .skip(1)
      .take_while(|p| p.starts_with(&workspace))
    {
      files.insert(parent.to_owned());
    }
    if reachable.contains(package["id"].as_str()?) {
      roots.insert(manifest.parent()?.to_owned());
    }
  }
  files.insert(workspace.join("Cargo.toml"));
  files.insert(workspace.join("Cargo.lock"));
  // Cargo searches config in the invocation directory and each ancestor.
  for ancestor in env::current_dir()
    .ok()?
    .ancestors()
    .chain(workspace.ancestors())
  {
    for name in [
      ".cargo/config",
      ".cargo/config.toml",
      "rust-toolchain",
      "rust-toolchain.toml",
    ] {
      files.insert(ancestor.join(name));
    }
  }
  let home = env::var_os("CARGO_HOME")
    .map(PathBuf::from)
    .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".cargo")))?;
  for name in ["config", "config.toml"] {
    files.insert(home.join(name));
  }
  let executable = env::current_exe().ok()?;
  files.insert(executable.clone());
  for name in ["flowistry-driver", ".flowistry-driver-wrapped"] {
    files.insert(executable.with_file_name(name));
  }
  for name in ["cargo", "rustc"] {
    files.insert(program(name)?);
  }
  for name in [
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC",
    "CC",
    "CXX",
    "AR",
  ] {
    if let Some(value) = env::var_os(name) {
      let path = PathBuf::from(&value);
      if path.is_file() {
        files.insert(path);
      } else if let Some(path) = value.to_str().and_then(program) {
        files.insert(path);
      }
    }
  }
  if let Some(sysroot) = env::var_os("SYSROOT") {
    files.insert(PathBuf::from(sysroot).join("bin/rustc"));
  }
  // Avoid walking nested package roots twice.
  let roots = roots
    .iter()
    .filter(|p| {
      !roots
        .iter()
        .any(|other| other != *p && p.starts_with(other))
    })
    .cloned()
    .collect();
  Some(Entry {
    key,
    workspace,
    roots,
    excluded: vec![target, directory.to_owned()],
    files: files.into_iter().collect(),
    snapshot: BTreeMap::new(),
    responses: vec![],
    checksum: String::new(),
  })
}

/// Capture include files and proc-macro declared inputs outside package trees.
/// Build-script rerun inputs are collected separately from Cargo's output files.
pub(crate) fn record_inputs(tcx: TyCtxt<'_>) {
  let Some(path) = env::var_os(CHILD) else {
    return;
  };
  if tcx
    .sess
    .env_depinfo
    .borrow()
    .iter()
    .any(|(key, _)| transient_env(key.as_str()))
  {
    return;
  }
  let mut files = BTreeSet::new();
  for file in tcx.sess.source_map().files().iter() {
    if let rustc_span::FileName::Real(name) = &file.name {
      if let Some(path) = name.local_path() {
        if path.is_file() {
          if let Ok(path) = path.canonicalize() {
            files.insert(path);
          }
        }
      }
    }
  }
  files.extend(
    tcx
      .sess
      .file_depinfo
      .borrow()
      .iter()
      .map(|p| PathBuf::from(p.as_str()))
      .map(|p| {
        if p.is_absolute() {
          p
        } else {
          env::current_dir().unwrap_or_default().join(p)
        }
      }),
  );
  if let Ok(data) = serde_json::to_vec(&files) {
    let _ = fs::write(path, data);
  }
}

fn build_inputs(entry: &mut Entry) -> Option<()> {
  // Cargo stores build-script directives in target/**/build/*/output. Watch
  // generated files too, plus explicitly declared inputs outside the package.
  fn visit(path: &Path, entry: &mut Entry) -> Option<()> {
    for child in fs::read_dir(path).ok()? {
      let child = child.ok()?.path();
      if child.is_dir() {
        if fs::symlink_metadata(&child).ok()?.file_type().is_symlink() {
          return None;
        }
        if child
          .file_name()
          .is_some_and(|n| n == "incremental" || n == ".fingerprint")
        {
          continue;
        }
        visit(&child, entry)?;
      } else if child.extension().is_some_and(|n| n == "d") {
        // Dependency-crate includes are absent from the selected SourceMap.
        let text = fs::read_to_string(&child).ok()?;
        if text
          .lines()
          .filter_map(|line| line.strip_prefix("# env-dep:"))
          .any(|input| transient_env(input.split('=').next().unwrap_or(input)))
        {
          return None;
        }
        let line = text.lines().next()?;
        let (_, inputs) = line.split_once(": ")?;
        let mut input = String::new();
        let mut escaped = false;
        for c in inputs.chars().chain(std::iter::once(' ')) {
          if escaped {
            input.push(c);
            escaped = false;
          } else if c == '\\' {
            escaped = true;
          } else if c == ' ' {
            if !input.is_empty() {
              let path = PathBuf::from(std::mem::take(&mut input).replace("$$", "$"));
              entry.files.push(if path.is_absolute() {
                path
              } else {
                entry.workspace.join(path)
              });
            }
          } else {
            input.push(c);
          }
        }
        if escaped {
          return None;
        }
        entry.files.push(child);
      } else if child.file_name().is_some_and(|n| n == "output") {
        entry.files.push(child.clone());
        // Cargo's root-output identifies OUT_DIR, which must remain unchanged.
        let out = child.with_file_name("out");
        if out.is_dir() {
          entry.roots.push(out);
        }
        let text = fs::read_to_string(&child).ok()?;
        for line in text.lines() {
          if line
            .strip_prefix("cargo:rerun-if-env-changed=")
            .or_else(|| line.strip_prefix("cargo::rerun-if-env-changed="))
            .is_some_and(transient_env)
          {
            return None;
          }
          if let Some(path) = line
            .strip_prefix("cargo:rerun-if-changed=")
            .or_else(|| line.strip_prefix("cargo::rerun-if-changed="))
          {
            let path = PathBuf::from(path);
            if path.is_absolute() {
              if path.is_dir() {
                entry.roots.push(path);
              } else {
                entry.files.push(path);
              }
            } else {
              // Relative directives are interpreted in the package directory.
              // Watch each candidate; absent files are recorded as absent.
              for root in entry.roots.clone() {
                let p = root.join(&path);
                if p.is_dir() {
                  entry.roots.push(p);
                } else {
                  entry.files.push(p);
                }
              }
            }
          }
        }
      }
    }
    Some(())
  }
  let target = entry.excluded.first()?.clone();
  if target.is_dir() {
    visit(&target, entry)?;
  }
  // Generated roots must not be excluded by the normal target-tree skip.
  // Add their files individually while retaining a directory stamp for additions.
  fn generated(path: &Path, files: &mut Vec<PathBuf>) -> Option<()> {
    if fs::symlink_metadata(path).ok()?.file_type().is_symlink() {
      return None;
    }
    files.push(path.to_owned());
    if path.is_dir() {
      for child in fs::read_dir(path).ok()? {
        generated(&child.ok()?.path(), files)?;
      }
    }
    Some(())
  }
  let roots = std::mem::take(&mut entry.roots);
  for root in roots {
    if root.starts_with(&target) {
      generated(&root, &mut entry.files)?;
    } else {
      entry.roots.push(root);
    }
  }
  entry.roots.sort();
  entry.roots.dedup();
  entry.files.sort();
  entry.files.dedup();
  Some(())
}

fn decode(data: &[u8]) -> Option<Value> {
  let bytes = base64::engine::general_purpose::STANDARD
    .decode(std::str::from_utf8(data).ok()?.trim())
    .ok()?;
  let mut text = Vec::new();
  flate2::read::GzDecoder::new(&bytes[..])
    .take(LIMIT + 1)
    .read_to_end(&mut text)
    .ok()?;
  if text.len() as u64 > LIMIT {
    return None;
  }
  let value: Value = serde_json::from_slice(&text).ok()?;
  let bodies = value["Ok"]["bodies"].as_array()?;
  if !bodies.iter().any(|b| b["focus"].get("Ok").is_some())
    || bodies.iter().any(|b| b["focus"].get("Err").is_some())
  {
    return None;
  }
  Some(value)
}
fn encode(value: &Value) -> Option<String> {
  let mut encoder =
    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
  serde_json::to_writer(&mut encoder, value).ok()?;
  Some(base64::engine::general_purpose::STANDARD.encode(encoder.finish().ok()?))
}
#[derive(Serialize, Deserialize)]
struct Response {
  bodies: Vec<((u64, u64), (u64, u64), bool)>,
  output: String,
}
impl Response {
  fn supports(&self, position: Option<(u64, u64)>) -> bool {
    if let Some(pos) = position {
      self
        .bodies
        .iter()
        // rustc's containment of a zero-width cursor span includes its end.
        // Match fresh file-focus selection, including nested closure boundaries.
        .filter(|(a, z, _)| *a <= pos && pos <= *z)
        .max_by_key(|(a, z, _)| (*a, std::cmp::Reverse(*z)))
        .is_some_and(|(_, _, analyzed)| *analyzed)
    } else {
      self.bodies.iter().all(|(_, _, analyzed)| *analyzed)
    }
  }
}

fn prepare(mut value: Value) -> Option<Response> {
  let bodies = value["Ok"]["bodies"].as_array_mut()?;
  let point = |p: &Value| Some((p["line"].as_u64()?, p["column"].as_u64()?));
  let ranges = bodies
    .iter()
    .map(|b| {
      Some((
        point(&b["range"]["start"])?,
        point(&b["range"]["end"])?,
        b["focus"].get("Ok").is_some(),
      ))
    })
    .collect::<Option<Vec<_>>>()?;
  let mut hits = 0;
  for body in bodies.iter_mut() {
    if body["focus"].get("Ok").is_some() {
      body["cached"] = true.into();
      hits += 1;
    }
  }
  value["Ok"]["cache"] =
    serde_json::json!({"hits": hits, "misses": 0, "validation": "snapshot"});
  Some(Response {
    bodies: ranges,
    output: encode(&value)?,
  })
}
fn load(path: &Path, key: &str) -> Option<Entry> {
  if fs::metadata(path).ok()?.len() > LIMIT {
    return None;
  }
  let entry: Entry = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
  (entry.key == key && entry.checksum == entry.checksum()?).then_some(entry)
}
fn save(path: &Path, entry: &mut Entry) -> Option<()> {
  entry.checksum = entry.checksum()?;
  let bytes = serde_json::to_vec(entry).ok()?;
  if bytes.len() as u64 > LIMIT {
    return None;
  }
  fs::create_dir_all(path.parent()?).ok()?;
  let temp = path.with_extension(format!("{}.tmp", std::process::id()));
  let result = fs::write(&temp, bytes).and_then(|_| fs::rename(&temp, path));
  let _ = fs::remove_file(temp);
  result.ok()?;
  let mut entries = fs::read_dir(path.parent()?)
    .ok()?
    .filter_map(Result::ok)
    .filter_map(|e| {
      let m = e.metadata().ok()?;
      Some((m.modified().ok()?, m.len(), e.path()))
    })
    .collect::<Vec<_>>();
  entries.sort();
  let mut total: u64 = entries.iter().map(|e| e.1).sum();
  let mut count = entries.len();
  for (_, len, old) in entries {
    if total <= 256 * 1024 * 1024 && count <= 256 {
      break;
    }
    if old != path && fs::remove_file(old).is_ok() {
      total -= len;
      count -= 1;
    }
  }
  Some(())
}

fn cached_run() -> Option<ExitCode> {
  let args = env::args().skip(1).collect::<Vec<_>>();
  let index = args.iter().position(|a| a == "file-focus")?;
  let position = match args.len() - index {
    2 => None,
    4 => Some((args[index + 2].parse().ok()?, args[index + 3].parse().ok()?)),
    _ => return None,
  };
  let directory = directory()?;
  let directory = if directory.is_absolute() {
    directory
  } else {
    env::current_dir().ok()?.join(directory)
  };
  let mode = env::var("FLOWISTRY_CACHE").unwrap_or_default();
  if mode == "off" {
    return None;
  }
  let environment: BTreeMap<_, _> = env::vars_os()
    .filter(|(k, _)| !k.to_str().is_some_and(transient_env))
    .collect();
  let key = digest(&(
    6u32,
    env::current_dir().ok()?,
    env::current_exe().ok()?,
    &args[.. index + 2],
    environment,
  ));
  let path = directory.join("responses-v1").join(format!("{key}.json"));
  let mut previous = load(&path, &key).filter(|e| {
    e.snapshot()
      .is_some_and(|current| same_contents(&current, &e.snapshot))
  });
  if mode != "refresh" {
    if let Some(entry) = &previous {
      for response in &entry.responses {
        if response.supports(position) {
          print!("{}", response.output);
          return Some(ExitCode::SUCCESS);
        }
      }
    }
  }
  let mut entry = previous
    .take()
    .or_else(|| discover(key, &directory, Path::new(&args[index + 1])))?;
  log::debug!("fast cache: snapshot {} package roots", entry.roots.len());
  fs::create_dir_all(&directory).ok()?;
  fs::create_dir_all(entry.excluded.first()?).ok()?;
  entry.snapshot = entry.snapshot()?;
  let nonce = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .ok()?
    .as_nanos();
  let inputs = directory.join(format!("inputs-{}-{nonce}.tmp", std::process::id()));
  let output = Command::new(env::current_exe().ok()?)
    .args(&args)
    .env(CHILD, &inputs)
    .stderr(Stdio::inherit())
    .output()
    .ok()?;
  // Preserve the protocol and error status even when optional caching fails.
  let _ = std::io::stdout().write_all(&output.stdout);
  let store_result = || -> Option<()> {
    if !output.status.success() || !same_contents(&entry.snapshot()?, &entry.snapshot) {
      log::debug!("fast cache: inputs changed during analysis");
      return None;
    }
    let response = prepare(decode(&output.stdout)?)?;
    log::debug!("fast cache: decoded response");
    let extra: Vec<PathBuf> = serde_json::from_slice(&fs::read(&inputs).ok()?).ok()?;
    log::debug!("fast cache: compiler recorded {} inputs", extra.len());
    entry.files.extend(extra);
    build_inputs(&mut entry)?;
    log::debug!("fast cache: collected build inputs");
    entry.snapshot = entry.snapshot()?;
    if mode == "refresh" {
      entry.responses.clear();
    }
    entry.responses.push(response);
    if entry.responses.len() > 128 {
      entry.responses.remove(0);
    }
    save(&path, &mut entry)
  };
  let mut store_result = store_result;
  let saved = store_result().is_some();
  log::debug!("fast cache: saved response: {saved}");
  let _ = fs::remove_file(inputs);
  Some(ExitCode::from(
    output
      .status
      .code()
      .and_then(|c| u8::try_from(c).ok())
      .unwrap_or(1),
  ))
}

pub fn run() -> ExitCode {
  if env::var_os(CHILD).is_none() {
    if let Some(code) = cached_run() {
      return code;
    }
  }
  // A snapshot miss must run Cargo so its new dep-info and build-script inputs
  // are recorded. Other requests can still reuse a validated driver invocation.
  if env::var_os(CHILD).is_some() {
    return rustc_plugin::cli_main(crate::FlowistryPlugin);
  }
  let request = crate::replay_request();
  if let Some((args, file)) = &request {
    if let Some(status) = crate::try_replay(file, args) {
      return status;
    }
  }
  let pending = request.and_then(|(_, file)| crate::prepare_replay(&file));
  let result = rustc_plugin::cli_main(crate::FlowistryPlugin);
  if let Some(pending) = pending {
    let _ = fs::remove_file(pending);
  }
  result
}
