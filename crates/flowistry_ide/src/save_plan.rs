//! Bounded scheduling observations in the shared store. These are hints, never
//! proof that a result is current. Every inventoried body still gets a worker.
use std::{
  collections::{BTreeMap, BTreeSet, VecDeque},
  hash::Hash,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
  cache::fingerprint,
  result_store::{Namespace, Store},
};

const SCHEMA: u32 = 1;

fn digest(value: &impl Serialize) -> String {
  fingerprint(|h| {
    serde_json::to_vec(value)
      .expect("scheduling record serializes")
      .hash(h)
  })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observation {
  schema: u32,
  context: String,
  mode: String,
  identity: String,
  expanded: String,
  callees: Vec<String>,
  semantic_key: String,
  integrity: String,
}

impl Observation {
  pub fn new(
    context: String,
    mode: String,
    identity: String,
    expanded: String,
    mut callees: Vec<String>,
    semantic_key: String,
  ) -> Self {
    callees.sort();
    callees.dedup();
    let mut value = Self {
      schema: SCHEMA,
      context,
      mode,
      identity,
      expanded,
      callees,
      semantic_key,
      integrity: String::new(),
    };
    value.integrity = value.checksum();
    value
  }

  fn checksum(&self) -> String {
    digest(&(
      self.schema,
      &self.context,
      &self.mode,
      &self.identity,
      &self.expanded,
      &self.callees,
      &self.semantic_key,
    ))
  }

  fn key(context: &str, mode: &str, identity: &str) -> String {
    format!("plan-{}", digest(&(SCHEMA, context, mode, identity)))
  }

  pub fn write(&self, store: &Store) -> std::io::Result<()> {
    store.put(
      Namespace::Dependencies,
      &Self::key(&self.context, &self.mode, &self.identity),
      &serde_json::to_vec(self)?,
      None,
    )?;
    Ok(())
  }

  fn read(store: &Store, context: &str, mode: &str, identity: &str) -> Option<Self> {
    let bytes = store
      .get(Namespace::Dependencies, &Self::key(context, mode, identity))
      .ok()?;
    Self::decode(&bytes, context, mode, identity)
  }

  fn decode(bytes: &[u8], context: &str, mode: &str, identity: &str) -> Option<Self> {
    let value: Self = serde_json::from_slice(bytes).ok()?;
    (value.schema == SCHEMA
      && value.context == context
      && value.mode == mode
      && value.identity == identity
      && value.integrity == value.checksum())
    .then_some(value)
  }
}

/// All compiler queries must finish before acquiring the store's publication lock.
pub(crate) fn inventory(
  context: &str,
  mode: &str,
  recurse: bool,
  expanded: &BTreeMap<String, String>,
) -> BTreeMap<String, Value> {
  let observations = crate::cache::store_root()
    .and_then(|root| Store::open(&root).ok())
    .map(|store| {
      expanded
        .keys()
        .filter_map(|id| {
          Observation::read(&store, context, mode, id).map(|value| (id.clone(), value))
        })
        .collect()
    })
    .unwrap_or_default();
  classify(expanded, &observations, recurse)
}

fn classify(
  expanded: &BTreeMap<String, String>,
  observations: &BTreeMap<String, Observation>,
  recurse: bool,
) -> BTreeMap<String, Value> {
  let mut result = BTreeMap::new();
  let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
  let mut queue = VecDeque::new();
  let mut seen = BTreeSet::new();
  for (id, hash) in expanded {
    let (status, reason) = match observations.get(id) {
      None => ("unknown", "missing-or-invalid-observation"),
      Some(old) if old.expanded != *hash => ("changed", "expanded-body-changed"),
      Some(old) if recurse && old.callees.iter().any(|id| !expanded.contains_key(id)) => {
        ("unknown", "callee-outside-inventory")
      }
      Some(_) => ("unchanged-input-hint", "matching-expanded-body"),
    };
    if status != "unchanged-input-hint" {
      queue.push_back(id.as_str());
    }
    result.insert(
      id.clone(),
      json!({"status":status, "reason":reason, "validated":false}),
    );
    if recurse {
      if let Some(old) = observations.get(id) {
        for callee in &old.callees {
          reverse.entry(callee).or_default().push(id);
        }
      }
    }
  }
  while let Some(id) = queue.pop_front() {
    if !seen.insert(id) {
      continue;
    }
    for caller in reverse.get(id).into_iter().flatten() {
      if result[*caller]["status"] == "unchanged-input-hint" {
        result.get_mut(*caller).unwrap()["status"] = json!("affected");
        result.get_mut(*caller).unwrap()["reason"] =
          json!("previous-resolved-callee-changed-or-unknown");
      }
      queue.push_back(caller);
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn changed_callee_propagates_through_cycles_only_in_recurse() {
    let expanded = BTreeMap::from([
      ("a".into(), "same".into()),
      ("b".into(), "edited".into()),
      ("c".into(), "same".into()),
      ("independent".into(), "same".into()),
    ]);
    let observations = [
      ("a", vec!["b"]),
      ("b", vec!["a"]),
      ("c", vec!["a"]),
      ("independent", vec![]),
    ]
    .into_iter()
    .map(|(id, callees)| {
      (
        id.into(),
        Observation::new(
          "context".into(),
          "mode".into(),
          id.into(),
          "same".into(),
          callees.into_iter().map(str::to_owned).collect(),
          "semantic".into(),
        ),
      )
    })
    .collect();
    let plan = classify(&expanded, &observations, true);
    for id in ["a", "c"] {
      assert_eq!(plan[id]["status"], "affected");
    }
    assert_eq!(plan["b"]["status"], "changed");
    assert_eq!(plan["independent"]["status"], "unchanged-input-hint");
    let plan = classify(&expanded, &observations, false);
    assert_eq!(plan["a"]["status"], "unchanged-input-hint");
    assert_eq!(plan["c"]["status"], "unchanged-input-hint");
  }

  #[test]
  fn missing_and_corrupt_observations_never_claim_validation() {
    let observation = Observation::new(
      "context".into(),
      "mode".into(),
      "root".into(),
      "body".into(),
      vec!["missing".into()],
      "semantic".into(),
    );
    let bytes = serde_json::to_vec(&observation).unwrap();
    assert!(Observation::decode(&bytes, "context", "mode", "root").is_some());
    assert!(Observation::decode(&bytes, "different-context", "mode", "root").is_none());
    assert!(Observation::decode(&bytes, "context", "other-mode", "root").is_none());
    let mut corrupt = observation.clone();
    corrupt.callees.clear();
    assert!(
      Observation::decode(
        &serde_json::to_vec(&corrupt).unwrap(),
        "context",
        "mode",
        "root"
      )
      .is_none()
    );
    let expanded = BTreeMap::from([
      ("root".into(), "body".into()),
      ("new".into(), "body".into()),
    ]);
    let plan = classify(
      &expanded,
      &BTreeMap::from([("root".into(), observation)]),
      true,
    );
    assert_eq!(plan["root"]["reason"], "callee-outside-inventory");
    assert_eq!(plan["new"]["reason"], "missing-or-invalid-observation");
    assert!(plan.values().all(|value| value["validated"] == false));
  }
}
