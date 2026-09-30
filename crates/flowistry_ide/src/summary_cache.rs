//! Compiler-validated semantic keys, portable summaries and dependency snapshots.
//!
//! Dependency records are immutable observations of one resolved closure, not a
//! claim that every body in the project is current. Consumers revalidate body
//! fingerprints and rebuild current edges; missing/evicted records mean a miss.
use std::{cell::RefCell, collections::BTreeMap, hash::Hash, path::PathBuf};

use flowistry::{
  extensions::{ContextMode, EvalMode},
  infoflow::{AnalysisSession, PortableSummary, SummaryStore},
};
use rustc_data_structures::fx::FxHashMap;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::{
  cache::{fingerprint, semantic_body},
  result_store::{Namespace, Store},
};

const SCHEMA: u32 = 1;

pub(crate) fn verify_summaries() -> bool {
  std::env::var("FLOWISTRY_VERIFY_SUMMARIES").as_deref() == Ok("1")
}

fn digest(value: &impl Serialize) -> Option<String> {
  let bytes = serde_json::to_vec(value).ok()?;
  Some(fingerprint(|h| bytes.hash(h)))
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
  name: String,
  semantic: String,
  callees: Vec<String>,
}

/// The immutable graph corresponding to one semantic key. Recurse includes the
/// complete reachable resolved-local closure; SigOnly includes just the root.
/// Its reverse edges permit conservative caller invalidation after body edits.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DependencySnapshot {
  schema: u32,
  context: String,
  mode: String,
  root: String,
  nodes: BTreeMap<String, Node>,
  callers: BTreeMap<String, Vec<String>>,
}

impl DependencySnapshot {
  fn reverse(nodes: &BTreeMap<String, Node>) -> Option<BTreeMap<String, Vec<String>>> {
    let mut callers: BTreeMap<_, Vec<_>> =
      nodes.keys().map(|id| (id.clone(), Vec::new())).collect();
    for (caller, node) in nodes {
      for callee in &node.callees {
        callers.get_mut(callee)?.push(caller.clone());
      }
    }
    for values in callers.values_mut() {
      values.sort();
      values.dedup();
    }
    Some(callers)
  }

  fn valid(&self, key: &str) -> bool {
    self.schema == SCHEMA
      && self.nodes.contains_key(&self.root)
      && Self::reverse(&self.nodes).as_ref() == Some(&self.callers)
      && digest(self).as_deref() == Some(key)
  }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
  schema: u32,
  key: String,
  payload: PortableSummary,
  integrity: String,
}
impl Entry {
  fn checksum(&self) -> Option<String> {
    digest(&(self.schema, &self.key, &self.payload))
  }
  fn decode(bytes: &[u8], key: &str) -> Option<PortableSummary> {
    let entry: Self = serde_json::from_slice(bytes).ok()?;
    (entry.schema == SCHEMA && entry.key == key && entry.checksum()? == entry.integrity)
      .then_some(entry.payload)
  }
}

pub(crate) struct SemanticCache<'tcx> {
  tcx: TyCtxt<'tcx>,
  root: Option<PathBuf>,
  context: String,
  refresh: bool,
  bodies: RefCell<FxHashMap<LocalDefId, String>>,
  keys: RefCell<FxHashMap<(LocalDefId, EvalMode), String>>,
}

impl<'tcx> SemanticCache<'tcx> {
  pub fn new(
    tcx: TyCtxt<'tcx>,
    root: Option<PathBuf>,
    context: String,
    refresh: bool,
  ) -> Self {
    Self {
      tcx,
      root,
      context,
      refresh,
      bodies: RefCell::default(),
      keys: RefCell::default(),
    }
  }

  fn identity(&self, def: LocalDefId) -> String {
    format!("{:?}", self.tcx.def_path_hash(def.to_def_id()))
  }

  fn body(&self, def: LocalDefId) -> String {
    if let Some(value) = self.bodies.borrow().get(&def) {
      return value.clone();
    }
    let value = semantic_body(self.tcx, def);
    self.bodies.borrow_mut().insert(def, value.clone());
    value
  }

  pub fn key(&self, session: &AnalysisSession<'tcx>, root: LocalDefId) -> String {
    let mode = session.mode();
    if let Some(key) = self.keys.borrow().get(&(root, mode)) {
      return key.clone();
    }
    let recurse = mode.context_mode == ContextMode::Recurse;
    let dependencies = if recurse {
      session.dependencies(root)
    } else {
      vec![root]
    };
    let nodes: BTreeMap<_, _> = dependencies
      .into_iter()
      .map(|def| {
        let mut callees = if recurse {
          session
            .direct_dependencies(def)
            .into_iter()
            .map(|callee| self.identity(callee))
            .collect::<Vec<_>>()
        } else {
          Vec::new()
        };
        callees.sort();
        callees.dedup();
        (self.identity(def), Node {
          name: self.tcx.def_path_str(def),
          semantic: self.body(def),
          callees,
        })
      })
      .collect();
    let callers = DependencySnapshot::reverse(&nodes)
      .expect("resolved dependency closure is complete");
    let graph = DependencySnapshot {
      schema: SCHEMA,
      context: self.context.clone(),
      mode: format!("{mode:?}"),
      root: self.identity(root),
      nodes,
      callers,
    };
    let key = digest(&graph).expect("dependency graph serializes");
    let observation = self.root.as_ref().map(|_| {
      crate::save_plan::Observation::new(
        self.context.clone(),
        graph.mode.clone(),
        graph.root.clone(),
        crate::cache::expanded_body(self.tcx, root),
        graph.nodes[&graph.root].callees.clone(),
        key.clone(),
      )
    });
    // All compiler queries above complete before acquiring the publication lock.
    if let Some(root) = &self.root {
      let result = (|| -> std::io::Result<()> {
        let store = Store::open(root)?;
        let existing = store
          .get(Namespace::Dependencies, &key)
          .ok()
          .and_then(|bytes| serde_json::from_slice::<DependencySnapshot>(&bytes).ok());
        if existing.is_none_or(|old| !old.valid(&key)) {
          store.put(
            Namespace::Dependencies,
            &key,
            &serde_json::to_vec(&graph)?,
            None,
          )?;
        }
        if let Some(observation) = &observation {
          observation.write(&store)?;
        }
        Ok(())
      })();
      if let Err(error) = result {
        log::debug!("Dependency snapshot write skipped: {error}");
      }
    }
    self.keys.borrow_mut().insert((root, mode), key.clone());
    key
  }
}

impl<'tcx> SummaryStore<'tcx> for SemanticCache<'tcx> {
  fn verify(&self) -> bool {
    verify_summaries()
  }
  fn load(
    &self,
    session: &AnalysisSession<'tcx>,
    callee: LocalDefId,
  ) -> Option<PortableSummary> {
    if self.refresh {
      return None;
    }
    let root = self.root.as_ref()?;
    let key = self.key(session, callee);
    let bytes = Store::open(root)
      .ok()?
      .get(Namespace::Summaries, &key)
      .ok()?;
    Entry::decode(&bytes, &key)
  }

  fn save(
    &self,
    session: &AnalysisSession<'tcx>,
    callee: LocalDefId,
    payload: &PortableSummary,
  ) {
    let Some(root) = &self.root else {
      return;
    };
    let key = self.key(session, callee);
    let mut entry = Entry {
      schema: SCHEMA,
      key,
      payload: payload.clone(),
      integrity: String::new(),
    };
    let Some(integrity) = entry.checksum() else {
      return;
    };
    entry.integrity = integrity;
    let result = (|| -> std::io::Result<()> {
      let bytes = serde_json::to_vec(&entry)?;
      // Like compiler-keyed focus entries, immutable summaries are reusable
      // after cancellation. They are never advertised as a current publication.
      Store::open(root)?.put(Namespace::Summaries, &entry.key, &bytes, None)?;
      Ok(())
    })();
    if let Err(error) = result {
      log::debug!("Summary cache write skipped: {error}");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reverse_edges_cover_cycles_and_reject_incomplete_graphs() {
    let node = |callees: &[&str]| Node {
      name: String::new(),
      semantic: "body".into(),
      callees: callees.iter().map(|name| name.to_string()).collect(),
    };
    let mut nodes = BTreeMap::from([
      ("a".into(), node(&["b"])),
      ("b".into(), node(&["a", "leaf"])),
      ("leaf".into(), node(&[])),
    ]);
    let callers = DependencySnapshot::reverse(&nodes).unwrap();
    assert_eq!(callers["leaf"], ["b"]);
    assert_eq!(callers["a"], ["b"]);
    assert_eq!(callers["b"], ["a"]);
    nodes.remove("leaf");
    assert!(DependencySnapshot::reverse(&nodes).is_none());
  }

  #[test]
  fn dependency_key_covers_mode_context_bodies_and_edges() {
    let mut graph = DependencySnapshot {
      schema: SCHEMA,
      context: "compiler/config/declarations".into(),
      mode: "Recurse".into(),
      root: "root".into(),
      nodes: BTreeMap::from([("root".into(), Node {
        name: "root".into(),
        semantic: "first body".into(),
        callees: vec![],
      })]),
      callers: BTreeMap::from([("root".into(), vec![])]),
    };
    let key = digest(&graph).unwrap();
    assert!(graph.valid(&key));
    graph.nodes.get_mut("root").unwrap().semantic = "edited body".into();
    assert!(!graph.valid(&key));
    let next = digest(&graph).unwrap();
    graph.mode = "SigOnly".into();
    assert!(!graph.valid(&next));
    let next = digest(&graph).unwrap();
    graph.context = "other compiler/config/declarations".into();
    assert!(!graph.valid(&next));
    graph.callers.get_mut("root").unwrap().push("absent".into());
    assert!(!graph.valid(&digest(&graph).unwrap()));
  }
}
