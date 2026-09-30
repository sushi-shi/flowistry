//! Storage shared by semantic results and their validated response index.
//!
//! One short-lived filesystem lock serializes publication, generation changes
//! and eviction. It is never held while running Cargo, rustc or the solver.
use std::{
  collections::BTreeMap,
  fs::{self, File, OpenOptions},
  hash::Hash,
  io::{self, Read, Write},
  path::{Component, Path, PathBuf},
};

use rustc_data_structures::{fingerprint::Fingerprint, stable_hasher::StableHasher};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_ENTRY: u64 = 32 * 1024 * 1024;
const DEFAULT_LIMIT: u64 = 256 * 1024 * 1024;
const MAX_EPOCHS: usize = 256;
const STATE_LIMIT: u64 = 1024 * 1024;
const STATE: &str = ".generations";

#[derive(Clone, Copy)]
pub(crate) enum Namespace {
  Focus,
  Responses,
}
impl Namespace {
  fn name(self) -> &'static str {
    match self {
      Self::Focus => "focus-v1",
      Self::Responses => "responses-v1",
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ticket {
  pub scope: String,
  pub revision: String,
  pub generation: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Generations {
  entries: BTreeMap<String, Ticket>,
  checksum: String,
}
impl Generations {
  fn checksum(&self) -> io::Result<String> {
    let mut hash = StableHasher::new();
    serde_json::to_vec(&self.entries)?.hash(&mut hash);
    let value: Fingerprint = hash.finish();
    Ok(format!("{value:?}"))
  }
}

pub(crate) struct Store {
  root: PathBuf,
  // Closing the file releases the process lock, including on unwinding/exit.
  _lock: File,
  limit: u64,
}

fn nonce() -> io::Result<String> {
  let mut bytes = [0; 16];
  File::open("/dev/urandom")?.read_exact(&mut bytes)?;
  Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn bounded_read(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
  let mut data = Vec::new();
  File::open(path)?.take(limit + 1).read_to_end(&mut data)?;
  if data.len() as u64 > limit {
    return Err(io::Error::other("result-store entry exceeds its limit"));
  }
  Ok(data)
}

impl Store {
  pub fn open(root: &Path) -> io::Result<Self> {
    let limit = std::env::var("FLOWISTRY_CACHE_MAX_BYTES")
      .ok()
      .and_then(|value| value.parse().ok())
      .unwrap_or(DEFAULT_LIMIT);
    Self::with_limit(root, limit)
  }

  fn with_limit(root: &Path, limit: u64) -> io::Result<Self> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(false)
      .mode(0o600)
      .open(root.join(".publication-lock"))?;
    lock.lock()?;
    Ok(Self {
      root: root.to_owned(),
      _lock: lock,
      limit,
    })
  }

  fn path(&self, namespace: Namespace, key: &str) -> io::Result<PathBuf> {
    let mut components = Path::new(key).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
      || components.next().is_some()
      || key.len() > 192
    {
      return Err(io::Error::other("invalid result-store key"));
    }
    Ok(self.root.join(namespace.name()).join(format!("{key}.json")))
  }

  pub fn get(&self, namespace: Namespace, key: &str) -> io::Result<Vec<u8>> {
    bounded_read(&self.path(namespace, key)?, MAX_ENTRY.min(self.limit))
  }

  fn generations(&self) -> Generations {
    (|| {
      let value: Generations =
        serde_json::from_slice(&bounded_read(&self.root.join(STATE), STATE_LIMIT).ok()?)
          .ok()?;
      (value.entries.len() <= MAX_EPOCHS
        && value
          .entries
          .iter()
          .all(|(scope, ticket)| scope == &ticket.scope)
        && value.checksum == value.checksum().ok()?)
      .then_some(value)
    })()
    .unwrap_or_default()
  }

  fn save_generations(&self, mut value: Generations) -> io::Result<()> {
    value.checksum = value.checksum()?;
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() as u64 > STATE_LIMIT || !self.write(&self.root.join(STATE), &bytes)? {
      return Err(io::Error::other(
        "generation state exceeds the result-store budget",
      ));
    }
    Ok(())
  }

  /// Requests for the same validated input revision can fill different bodies
  /// concurrently. A new revision or cancellation invalidates their old tickets.
  pub fn begin(&self, scope: &str, revision: &str) -> io::Result<Ticket> {
    if scope.len() > 192 || revision.len() > 192 {
      return Err(io::Error::other("invalid publication scope or revision"));
    }
    let mut state = self.generations();
    if let Some(ticket) = state.entries.get(scope).filter(|t| t.revision == revision) {
      return Ok(ticket.clone());
    }
    let ticket = Ticket {
      scope: scope.into(),
      revision: revision.into(),
      generation: nonce()?,
    };
    state.entries.insert(scope.into(), ticket.clone());
    // Eviction invalidates active tickets for the evicted scope. That is a safe
    // miss; never reuse a numeric generation after eviction, corruption or restart.
    while state.entries.len() > MAX_EPOCHS {
      let victim = state
        .entries
        .keys()
        .find(|key| key.as_str() != scope)
        .unwrap()
        .clone();
      state.entries.remove(&victim);
    }
    self.save_generations(state)?;
    Ok(ticket)
  }

  pub fn current(&self, ticket: &Ticket) -> bool {
    self.generations().entries.get(&ticket.scope) == Some(ticket)
  }

  pub fn cancel(&self, scope: &str) -> io::Result<()> {
    let mut state = self.generations();
    state.entries.remove(scope);
    self.save_generations(state)
  }

  pub fn put(
    &self,
    namespace: Namespace,
    key: &str,
    bytes: &[u8],
    ticket: Option<&Ticket>,
  ) -> io::Result<bool> {
    if ticket.is_some_and(|ticket| !self.current(ticket))
      || bytes.len() as u64 > MAX_ENTRY
    {
      return Ok(false);
    }
    self.write(&self.path(namespace, key)?, bytes)
  }

  fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt;
    if bytes.len() as u64 > self.limit {
      return Ok(false);
    }
    for item in fs::read_dir(&self.root)? {
      let item = item?;
      let name = item.file_name();
      if name
        .to_str()
        .is_some_and(|name| name.starts_with(".generations.") && name.ends_with(".tmp"))
      {
        let _ = fs::remove_file(item.path());
      }
    }
    let mut entries = Vec::new();
    for namespace in [Namespace::Focus, Namespace::Responses] {
      let directory = self.root.join(namespace.name());
      fs::create_dir_all(&directory)?;
      for item in fs::read_dir(directory)? {
        let item = item?;
        let p = item.path();
        if p.extension().is_some_and(|ext| ext == "tmp") {
          // All current writers hold this lock. Leftover temporaries belong to
          // interrupted writes, and cannot be valid published results.
          let _ = fs::remove_file(p);
        } else if p.extension().is_some_and(|ext| ext == "json") {
          let metadata = item.metadata()?;
          entries.push((metadata.modified()?, metadata.len(), p));
        }
      }
    }
    let state_path = self.root.join(STATE);
    let state_bytes = if path == state_path {
      0
    } else {
      fs::metadata(&state_path).map(|m| m.len()).unwrap_or(0)
    };
    let mut total = bytes.len() as u64
      + state_bytes
      + entries
        .iter()
        .filter(|(_, _, p)| p != path)
        .map(|(_, len, _)| *len)
        .sum::<u64>();
    let mut count =
      entries.len() + usize::from(!entries.iter().any(|(_, _, p)| p == path));
    entries.sort();
    for (_, length, old) in entries {
      if total <= self.limit && count <= 2048 {
        break;
      }
      if old != path {
        fs::remove_file(old)?;
        total -= length;
        count -= 1;
      }
    }
    if total > self.limit {
      return Ok(false);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension(format!("{}.tmp", nonce()?));
    let written = (|| {
      let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
      file.write_all(bytes)?;
      file.sync_all()?;
      fs::rename(&temp, path)
    })();
    let _ = fs::remove_file(temp);
    written?;
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct Fixture(PathBuf);
  impl Fixture {
    fn new() -> Self {
      Self(std::env::temp_dir().join(format!("flowistry-store-{}", nonce().unwrap())))
    }
    fn open(&self) -> Store {
      Store::with_limit(&self.0, 8192).unwrap()
    }
  }
  impl Drop for Fixture {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.0);
    }
  }

  #[test]
  fn revisions_cancellation_and_restart_reject_old_writers() {
    let fixture = Fixture::new();
    let old = fixture.open().begin("file", "A").unwrap();
    assert_eq!(old, fixture.open().begin("file", "A").unwrap());
    let changed = fixture.open().begin("file", "B").unwrap();
    let store = fixture.open();
    assert!(
      !store
        .put(Namespace::Responses, "old", b"stale", Some(&old))
        .unwrap()
    );
    assert!(
      store
        .put(Namespace::Responses, "new", b"current", Some(&changed))
        .unwrap()
    );
    store.cancel("file").unwrap();
    assert!(!store.current(&changed));
    let again = store.begin("file", "A").unwrap();
    assert_ne!(again.generation, old.generation);
    assert!(!store.current(&old));
  }

  #[test]
  fn corrupt_generations_do_not_resurrect_a_ticket() {
    let fixture = Fixture::new();
    let ticket = fixture.open().begin("file", "A").unwrap();
    fs::write(fixture.0.join(STATE), b"{interrupted").unwrap();
    let store = fixture.open();
    assert!(!store.current(&ticket));
    assert_ne!(
      ticket.generation,
      store.begin("file", "A").unwrap().generation
    );
  }

  #[test]
  fn concurrent_namespaces_share_one_disk_budget() {
    let fixture = Fixture::new();
    let workers = (0 .. 8)
      .map(|i| {
        let root = fixture.0.clone();
        std::thread::spawn(move || {
          let store = Store::with_limit(&root, 8192).unwrap();
          let ns = if i % 2 == 0 {
            Namespace::Focus
          } else {
            Namespace::Responses
          };
          assert!(store.put(ns, &i.to_string(), &vec![i; 2048], None).unwrap());
        })
      })
      .collect::<Vec<_>>();
    for worker in workers {
      worker.join().unwrap();
    }
    let bytes: u64 = [Namespace::Focus, Namespace::Responses]
      .iter()
      .flat_map(|ns| {
        fs::read_dir(fixture.0.join(ns.name()))
          .unwrap()
          .map(|e| e.unwrap().metadata().unwrap().len())
      })
      .sum();
    assert!(bytes <= 8192);
    let store = fixture.open();
    assert!(
      !store
        .put(Namespace::Focus, "huge", &[0; 8193], None)
        .unwrap()
    );
    assert!(
      store
        .put(Namespace::Focus, "../escape", b"x", None)
        .is_err()
    );
  }
}
