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

use crate::result_store::{Namespace, Store, Ticket};

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
      | "FLOWISTRY_RESULT_PROTOCOL"
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

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
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
  package: String,
  workspace: PathBuf,
  roots: Vec<PathBuf>,
  excluded: Vec<PathBuf>,
  files: Vec<PathBuf>,
  snapshot: Snapshot,
  responses: Vec<Response>,
  provenance: Option<Provenance>,
  published: Option<Ticket>,
  checksum: String,
}
impl Entry {
  fn unchanged_inputs(&self, current: &Snapshot) -> bool {
    let input = |path: &Path| {
      !self
        .excluded
        .first()
        .is_some_and(|target| path.starts_with(target))
    };
    self
      .snapshot
      .iter()
      .filter(|(path, _)| input(path))
      .all(|(path, value)| current.get(path) == Some(value))
      && current
        .keys()
        .filter(|path| input(path))
        .all(|path| self.snapshot.contains_key(path))
  }
  fn checksum(&self) -> Option<String> {
    Some(digest(
      &serde_json::to_vec(&(
        &self.key,
        &self.package,
        &self.workspace,
        &self.roots,
        &self.excluded,
        &self.files,
        &self.snapshot,
        &self.responses,
        &self.provenance,
        &self.published,
      ))
      .ok()?,
    ))
  }
  fn revision(&self) -> String {
    digest(&(
      &self.key,
      self
        .snapshot
        .iter()
        .map(|(path, input)| (path, input.as_ref().map(|input| &input.digest)))
        .collect::<Vec<_>>(),
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
    package: selected.to_owned(),
    workspace,
    roots,
    excluded: vec![target, directory.to_owned()],
    files: files.into_iter().collect(),
    snapshot: BTreeMap::new(),
    responses: vec![],
    provenance: None,
    published: None,
    checksum: String::new(),
  })
}

/// Capture include files and proc-macro declared inputs outside package trees.
/// Build-script rerun inputs are collected separately from Cargo's output files.
pub(crate) fn record_inputs(tcx: TyCtxt<'_>, bodies: Vec<BodyIdentity>) {
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
  let mut verified_sources = BTreeMap::new();
  for file in tcx.sess.source_map().files().iter() {
    if let rustc_span::FileName::Real(name) = &file.name {
      if let Some(path) = name.local_path() {
        if path.is_file() {
          if let Ok(path) = path.canonicalize() {
            let verified = (|| {
              let before = stamp(&path)?;
              let source = fs::File::open(&path).ok()?;
              if rustc_span::SourceFileHash::new(file.src_hash.kind, source).ok()?
                != file.src_hash
              {
                return None;
              }
              let digest = file_digest(&path)?;
              (stamp(&path)? == before).then_some(Input {
                stamp: before,
                digest,
              })
            })();
            if let Some(input) = verified {
              verified_sources.insert(path.clone(), Some(input));
            } else {
              // The source on disk no longer matches the bytes rustc parsed.
              return;
            }
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
  let provenance = Provenance {
    compiler: rustc_interface::util::rustc_version_str()
      .unwrap_or("unknown")
      .into(),
    crate_name: tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE).to_string(),
    crate_types: tcx
      .sess
      .opts
      .crate_types
      .iter()
      .map(|kind| format!("{kind:?}"))
      .collect(),
    target: format!("{:?}", tcx.sess.target),
    configuration: digest(&tcx.sess.opts.dep_tracking_hash(true)),
    mode: format!("{:?}", flowistry::extensions::EvalMode::from_ambient()),
    bodies,
  };
  if let Ok(data) = serde_json::to_vec(&CompilerInputs {
    files: files.into_iter().collect(),
    verified_sources,
    provenance,
  }) {
    if data.len() as u64 <= LIMIT {
      let _ = fs::write(path, data);
    }
  }
}

#[derive(Serialize, Deserialize)]
struct CompilerInputs {
  files: Vec<PathBuf>,
  verified_sources: Snapshot,
  provenance: Provenance,
}

#[derive(Serialize, Deserialize)]
struct Provenance {
  compiler: String,
  crate_name: String,
  crate_types: Vec<String>,
  target: String,
  configuration: String,
  mode: String,
  bodies: Vec<BodyIdentity>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct BodyIdentity {
  identity: String,
  name: String,
  range: Value,
}
impl BodyIdentity {
  pub fn new(
    tcx: TyCtxt<'_>,
    id: rustc_hir::BodyId,
    range: &rustc_utils::source_map::range::CharRange,
  ) -> Self {
    let def = tcx.hir_body_owner_def_id(id);
    Self {
      identity: format!("{:?}", tcx.def_path_hash(def.to_def_id())),
      name: tcx.def_path_str(def),
      range: serde_json::to_value(range).unwrap(),
    }
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
fn load(store: &Store, key: &str) -> Option<Entry> {
  let entry: Entry =
    serde_json::from_slice(&store.get(Namespace::Responses, key).ok()?).ok()?;
  (entry.key == key
    && entry.checksum == entry.checksum()?
    && entry
      .published
      .as_ref()
      .is_none_or(|ticket| ticket.scope == key && ticket.revision == entry.revision()))
  .then_some(entry)
}
fn save(store: &Store, entry: &mut Entry, ticket: &Ticket) -> Option<()> {
  entry.published = entry.provenance.as_ref().map(|_| ticket.clone());
  entry.checksum = entry.checksum()?;
  let bytes = serde_json::to_vec(entry).ok()?;
  store
    .put(Namespace::Responses, &entry.key, &bytes, Some(ticket))
    .ok()?
    .then_some(())
}

fn protocol() -> bool {
  env::var("FLOWISTRY_RESULT_PROTOCOL").is_ok_and(|value| value == "1")
}

fn emit(data: &[u8], status: &str, ticket: Option<&Ticket>) {
  if protocol() {
    let output =
      matches!(status, "current" | "uncached").then(|| String::from_utf8_lossy(data));
    println!(
      "{}",
      serde_json::json!({"schema": 1, "status": status,
      "revision": ticket.map(|t| &t.revision), "generation": ticket.map(|t| &t.generation),
      "output": output})
    );
  } else {
    let _ = std::io::stdout().write_all(data);
  }
}

fn index(entry: &Entry, ticket: &Ticket) -> Value {
  let point = |value: &Value| Some((value["line"].as_u64()?, value["column"].as_u64()?));
  let bodies = entry.provenance.as_ref().map(|provenance| provenance.bodies.iter().map(|body| {
    let available = (|| {
      // Coincident spans cannot safely identify which compiler body was selected.
      if provenance.bodies.iter().filter(|other| other.range == body.range).count() != 1 { return None; }
      let a = point(&body.range["start"])?;
      let z = point(&body.range["end"])?;
      Some(entry.responses.iter().any(|response| response.bodies.iter()
        .any(|&(start, end, analyzed)| start == a && end == z && analyzed)))
    })().unwrap_or(false);
    serde_json::json!({"identity": body.identity, "name": body.name, "range": body.range, "available": available})
  }).collect::<Vec<_>>()).unwrap_or_default();
  serde_json::json!({"schema": 1, "status": "current", "revision": ticket.revision,
    "generation": ticket.generation, "package": entry.package,
    "produced_by": entry.published,
    "validation": {"kind": "snapshot", "compiler": entry.provenance.as_ref().map(|p| &p.compiler),
      "crate": entry.provenance.as_ref().map(|p| &p.crate_name),
      "crate_types": entry.provenance.as_ref().map(|p| &p.crate_types),
      "target": entry.provenance.as_ref().map(|p| &p.target),
      "configuration": entry.provenance.as_ref().map(|p| &p.configuration),
      "mode": entry.provenance.as_ref().map(|p| &p.mode)}, "bodies": bodies})
}

fn cached_run() -> Option<ExitCode> {
  let mut args = env::args().skip(1).collect::<Vec<_>>();
  let command_index = args.iter().position(|a| {
    matches!(a.as_str(), "file-focus" | "result-index" | "cancel-results")
  })?;
  let operation = args[command_index].clone();
  let position = match args.len() - command_index {
    2 => None,
    4 if operation == "file-focus" => Some((
      args[command_index + 2].parse().ok()?,
      args[command_index + 3].parse().ok()?,
    )),
    _ => return None,
  };
  args[command_index] = "file-focus".into();
  args[command_index + 1] = Path::new(&args[command_index + 1])
    .canonicalize()
    .ok()?
    .to_str()?
    .into();
  let directory = directory()?;
  let directory = if directory.is_absolute() {
    directory
  } else {
    env::current_dir().ok()?.join(directory)
  };
  let mode = env::var("FLOWISTRY_CACHE").unwrap_or_default();
  if mode == "off" {
    if operation == "result-index" {
      println!(
        "{}",
        serde_json::json!({"schema": 1, "status": "miss", "bodies": []})
      );
      return Some(ExitCode::SUCCESS);
    }
    if operation != "cancel-results" {
      return None;
    }
  }
  let environment: BTreeMap<_, _> = env::vars_os()
    .filter(|(k, _)| !k.to_str().is_some_and(transient_env))
    .collect();
  let key = digest(&(
    7u32,
    env::current_dir().ok()?,
    env::current_exe().ok()?,
    &args[.. command_index + 2],
    environment,
  ));
  let store = Store::open(&directory).ok()?;
  if operation == "cancel-results" {
    store.cancel(&key).ok()?;
    println!("{}", serde_json::json!({"schema": 1, "status": "canceled"}));
    return Some(ExitCode::SUCCESS);
  }
  let mut previous = load(&store, &key).filter(|e| {
    e.snapshot()
      .is_some_and(|current| same_contents(&current, &e.snapshot))
  });
  if operation == "result-index" {
    let value =
      if let Some(entry) = previous.as_ref().filter(|entry| entry.provenance.is_some()) {
        let ticket = store.begin(&key, &entry.revision()).ok()?;
        index(entry, &ticket)
      } else {
        serde_json::json!({"schema": 1, "status": "miss", "bodies": []})
      };
    println!("{value}");
    return Some(ExitCode::SUCCESS);
  }
  if mode != "refresh" && !crate::summary_cache::verify_summaries() {
    if let Some(entry) = &previous {
      for response in &entry.responses {
        if response.supports(position) {
          let ticket = store.begin(&key, &entry.revision()).ok()?;
          emit(response.output.as_bytes(), "current", Some(&ticket));
          return Some(ExitCode::SUCCESS);
        }
      }
    }
  }
  drop(store);
  let mut entry = previous
    .take()
    .or_else(|| discover(key.clone(), &directory, Path::new(&args[command_index + 1])))?;
  log::debug!("fast cache: snapshot {} package roots", entry.roots.len());
  fs::create_dir_all(&directory).ok()?;
  fs::create_dir_all(entry.excluded.first()?).ok()?;
  build_inputs(&mut entry)?;
  entry.snapshot = entry.snapshot()?;
  let ticket = Store::open(&directory)
    .ok()?
    .begin(&key, &entry.revision())
    .ok()?;
  let nonce = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .ok()?
    .as_nanos();
  let inputs = directory.join(format!("inputs-{}-{nonce}.tmp", std::process::id()));
  // The compiler can open the parent's anonymous file through procfs. Removing
  // its name before launch means SIGKILL/cancellation cannot leak input sidecars.
  use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
  let input_file = fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create_new(true)
    .mode(0o600)
    .open(&inputs)
    .ok()?;
  fs::remove_file(&inputs).ok()?;
  let inputs = PathBuf::from(format!(
    "/proc/{}/fd/{}",
    std::process::id(),
    input_file.as_raw_fd()
  ));
  let output = Command::new(env::current_exe().ok()?)
    .args(&args)
    .env(CHILD, &inputs)
    .stderr(Stdio::inherit())
    .output()
    .ok()?;
  let mut publication = None;
  let mut rejection = "uncached";
  let store_result = || -> Option<()> {
    // During an in-flight analysis, even a same-content rewrite can be an ABA
    // edit. Warm reads can accept equal contents; writers require unchanged stamps.
    if !output.status.success() || !entry.unchanged_inputs(&entry.snapshot()?) {
      rejection = "superseded";
      log::debug!("fast cache: inputs changed during analysis");
      return None;
    }
    let response = prepare(decode(&output.stdout)?)?;
    log::debug!("fast cache: decoded response");
    let preflight = entry.snapshot.clone();
    let extra: CompilerInputs = serde_json::from_slice(&fs::read(&inputs).ok()?).ok()?;
    log::debug!("fast cache: compiler recorded {} inputs", extra.files.len());
    entry.files.extend(extra.files);
    entry.provenance = Some(extra.provenance);
    build_inputs(&mut entry)?;
    log::debug!("fast cache: collected build inputs");
    entry.snapshot = entry.snapshot()?;
    if preflight.iter().any(|(path, input)| {
      !entry
        .excluded
        .first()
        .is_some_and(|target| path.starts_with(target))
        && entry.snapshot.get(path) != Some(input)
    }) || extra
      .verified_sources
      .iter()
      .any(|(path, input)| entry.snapshot.get(path) != Some(input))
    {
      rejection = "superseded";
      return None;
    }
    let store = Store::open(&directory).ok()?;
    if !store.current(&ticket) {
      rejection = "superseded";
      return None;
    }
    // Recheck after waiting for the publication lock, then refine the revision
    // with compiler/Cargo-discovered inputs using the existing validation rules.
    if entry.snapshot()? != entry.snapshot {
      rejection = "superseded";
      return None;
    }
    let final_ticket = store.begin(&key, &entry.revision()).ok()?;
    // Newly discovered external inputs were not watched before Cargo/rustc.
    // Accept them only when rustc's original source hash proves what was read.
    // Otherwise retain the watch list, but no result, for a second validated run.
    // Files Cargo generated inside its target directory are outputs of the
    // already-watched build; external build/dependency inputs are not exempt.
    let new_unknown = entry.snapshot.iter().any(|(path, input)| {
      input.is_some()
        && !preflight.contains_key(path)
        && !entry
          .excluded
          .first()
          .is_some_and(|target| path.starts_with(target))
        && extra.verified_sources.get(path) != Some(input)
    });
    if new_unknown {
      log::debug!("fast cache: external input requires a validated second run");
      entry.responses.clear();
      entry.provenance = None;
      save(&store, &mut entry, &final_ticket)?;
      return None;
    }
    if let Some(latest) =
      load(&store, &key).filter(|old| same_contents(&old.snapshot, &entry.snapshot))
    {
      entry.responses = latest.responses;
    }
    entry.responses.retain(|old| old.bodies != response.bodies);
    entry.responses.push(response);
    if entry.responses.len() > 128 {
      entry.responses.remove(0);
    }
    let result = save(&store, &mut entry, &final_ticket);
    publication = Some(final_ticket);
    result
  };
  let mut store_result = store_result;
  let saved = store_result().is_some();
  log::debug!("fast cache: saved response: {saved}");
  drop(input_file);
  let state = if !output.status.success() {
    "error"
  } else if publication.is_some() {
    "current"
  } else {
    rejection
  };
  emit(
    &output.stdout,
    state,
    publication.as_ref().or(Some(&ticket)),
  );
  if protocol() && state == "superseded" {
    return Some(ExitCode::from(75));
  }
  Some(crate::replay::child_exit_code(output.status))
}

pub fn run() -> ExitCode {
  if env::var_os(CHILD).is_none() {
    if let Some(code) = cached_run() {
      return code;
    }
    if crate::plugin::result_control_request() {
      println!(
        "{}",
        serde_json::json!({"schema": 1, "status": "unavailable",
        "error": "result store unavailable, invalid arguments, or file not found"})
      );
      return ExitCode::FAILURE;
    }
    if protocol() {
      // Unsupported snapshots still run normally, but cannot claim a validated
      // input revision. The negotiated protocol makes that distinction explicit.
      match Command::new(env::current_exe().unwrap())
        .args(env::args().skip(1))
        .env_remove("FLOWISTRY_RESULT_PROTOCOL")
        .stderr(Stdio::inherit())
        .output()
      {
        Ok(output) => {
          emit(
            &output.stdout,
            if output.status.success() {
              "uncached"
            } else {
              "error"
            },
            None,
          );
          return crate::replay::child_exit_code(output.status);
        }
        Err(error) => {
          eprintln!("flowistry: {error}");
          return ExitCode::FAILURE;
        }
      }
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

#[cfg(test)]
mod publication_tests {
  use super::*;

  #[test]
  fn source_aba_is_rejected_while_cargo_can_replace_its_outputs() {
    let input = Input {
      stamp: Stamp {
        len: 1,
        modified: 1,
        changed: (1, 0),
        inode: 1,
      },
      digest: "same bytes".into(),
    };
    let source = PathBuf::from("/project/src/lib.rs");
    let generated = PathBuf::from("/project/target/build/output");
    let entry = Entry {
      key: "key".into(),
      package: "package".into(),
      workspace: "/project".into(),
      roots: vec![],
      excluded: vec!["/project/target".into()],
      files: vec![],
      snapshot: [
        (source.clone(), Some(input.clone())),
        (generated.clone(), Some(input.clone())),
      ]
      .into(),
      responses: vec![],
      provenance: None,
      published: None,
      checksum: String::new(),
    };
    let mut now = entry.snapshot.clone();
    now
      .get_mut(&generated)
      .unwrap()
      .as_mut()
      .unwrap()
      .stamp
      .changed
      .0 += 1;
    assert!(entry.unchanged_inputs(&now));
    now
      .get_mut(&source)
      .unwrap()
      .as_mut()
      .unwrap()
      .stamp
      .changed
      .0 += 1;
    assert!(!entry.unchanged_inputs(&now));
    // Content equality remains appropriate for an already-published warm read,
    // but cannot prove which contents an in-flight compiler saw during an ABA edit.
    assert!(same_contents(&now, &entry.snapshot));
  }
}
