//! Conservative whitespace proof for the existing snapshot response store.
//! No macro/build observer or external crate may participate in this first
//! supported subset. Comments remain tokens, including their exact bytes.
use rustc_utils::source_map::range::CharPos;

use super::*;

const MAX_SOURCE: usize = 1024 * 1024;
const MAX_SOURCES: usize = 8 * MAX_SOURCE;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Proof {
  sources: BTreeMap<PathBuf, String>,
}

pub(super) fn metadata_reason(metadata: &Value, selected: &str) -> Option<String> {
  let eligible = (|| {
    let node = metadata["resolve"]["nodes"]
      .as_array()?
      .iter()
      .find(|node| node["id"].as_str() == Some(selected))?;
    if !node["dependencies"].as_array()?.is_empty() {
      return None;
    }
    let package = metadata["packages"]
      .as_array()?
      .iter()
      .find(|package| package["id"].as_str() == Some(selected))?;
    for target in package["targets"].as_array()? {
      for kind in target["kind"].as_array()? {
        if matches!(kind.as_str()?, "custom-build" | "proc-macro") {
          return None;
        }
      }
    }
    Some(())
  })()
  .is_some();
  (!eligible).then(|| "dependencies, build scripts or procedural targets".into())
}

/// Token spelling/kind and punctuation jointness form the syntactic proof.
/// In particular `> >` must not become `>>` just because the lexer emits two
/// tokens for both spellings. Rustfmt changes that join/split punctuation miss.
fn tokens(text: &str) -> Result<Vec<(String, String, bool)>, &'static str> {
  if text.len() > MAX_SOURCE || text.contains(['\r', '\u{feff}']) {
    return Err("source size or normalization");
  }
  let mut offset = 0;
  let mut previous_end = 0;
  let mut previous_punctuation = false;
  let mut result = Vec::new();
  for token in rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No) {
    let start = offset;
    offset += token.len as usize;
    let spelling = &text[start .. offset];
    if matches!(token.kind, rustc_lexer::TokenKind::Whitespace) {
      if !spelling.chars().all(|c| matches!(c, ' ' | '\t' | '\n')) {
        return Err("unsupported whitespace");
      }
      continue;
    }
    let identifier = spelling.strip_prefix("r#").unwrap_or(spelling);
    if matches!(identifier, "extern" | "Location" | "caller_location")
      || matches!(spelling, "!" | "#")
      || matches!(
        token.kind,
        rustc_lexer::TokenKind::LineComment {
          doc_style: Some(_),
          ..
        } | rustc_lexer::TokenKind::BlockComment {
          doc_style: Some(_),
          ..
        }
      )
    {
      return Err("macros, attributes, external linkage or source-location access");
    }
    let punctuation = spelling.len() == 1
      && spelling.as_bytes()[0].is_ascii_punctuation()
      && spelling != "_";
    let joint = punctuation && previous_punctuation && previous_end == start;
    result.push((format!("{:?}", token.kind), spelling.to_owned(), joint));
    previous_end = offset;
    previous_punctuation = punctuation;
  }
  Ok(result)
}

fn source_digest(text: &str) -> String {
  let mut h = StableHasher::new();
  h.write(text.as_bytes());
  let hash: Fingerprint = h.finish();
  format!("{hash:?}")
}

pub(super) fn capture(entry: &Entry, verified: &Snapshot) -> Result<Proof, &'static str> {
  if entry.layout_blocked.is_some() {
    return Err("Cargo target has source observers");
  }
  if !entry.provenance.as_ref().is_some_and(|p| p.layout_safe) {
    return Err("compiler-injected attributes");
  }
  // Configuration can install wrappers or inject crate attributes. Do not try
  // to infer which subset of arbitrary Cargo configuration is harmless.
  if entry.snapshot.iter().any(|(path, input)| {
    input.is_some()
      && matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("config" | "config.toml")
      )
  }) || [
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
  ]
  .iter()
  .any(|name| env::var_os(name).is_some_and(|v| !v.is_empty()))
    || env::vars_os().any(|(key, value)| {
      key.to_str().is_some_and(|key| {
        key.ends_with("_RUSTFLAGS")
          || key.ends_with("_RUSTC_WRAPPER")
          || key.ends_with("_RUSTC_WORKSPACE_WRAPPER")
      }) && !value.is_empty()
    })
    || ["RUSTC", "CARGO_BUILD_RUSTC"].iter().any(|name| {
      env::var_os(name).is_some_and(|value| {
        PathBuf::from(value).canonicalize().ok()
          != program("rustc").and_then(|p| p.canonicalize().ok())
      })
    })
  {
    return Err("custom compiler configuration");
  }
  let mut sources = BTreeMap::new();
  let mut size = 0;
  for (path, input) in verified {
    if path.extension().is_none_or(|e| e != "rs")
      || !entry.roots.iter().any(|root| path.starts_with(root))
    {
      return Err("external or non-Rust compiler source");
    }
    let input = input.as_ref().ok_or("missing compiler source")?;
    if input.stamp.len > MAX_SOURCE as u64 {
      return Err("source size");
    }
    let text = fs::read_to_string(path).map_err(|_| "unreadable source")?;
    size += text.len();
    if size > MAX_SOURCES {
      return Err("aggregate source size");
    }
    if source_digest(&text) != input.digest {
      return Err("source changed during proof");
    }
    tokens(&text)?;
    sources.insert(path.clone(), text);
  }
  if sources.is_empty() {
    return Err("no compiler-attested sources");
  }
  Ok(Proof { sources })
}

type Relocations = BTreeMap<PathBuf, (crate::cache::Source, crate::cache::Source)>;

fn relocate_value(
  value: &mut Value,
  files: &Value,
  sources: &Relocations,
) -> Result<(), &'static str> {
  match value {
    Value::Object(object)
      if object.contains_key("filename")
        && object.contains_key("start")
        && object.contains_key("end") =>
    {
      let filename = &object["filename"];
      let name = filename
        .as_str()
        .or_else(|| {
          filename
            .as_u64()
            .and_then(|id| files.get(id.to_string()))
            .and_then(Value::as_str)
        })
        .ok_or("unresolved output filename")?;
      let path = Path::new(name)
        .canonicalize()
        .map_err(|_| "missing output source")?;
      if let Some((old, new)) = sources.get(&path) {
        let point = |p: &Value| {
          Some(CharPos {
            line: usize::try_from(p["line"].as_u64()?).ok()?,
            column: usize::try_from(p["column"].as_u64()?).ok()?,
          })
        };
        let start = point(&object["start"]).ok_or("invalid start")?;
        let end = point(&object["end"]).ok_or("invalid end")?;
        let (start, end) = old.relocate(new, start, end).ok_or("unanchored range")?;
        object.insert(
          "start".into(),
          serde_json::json!({"line": start.line, "column": start.column}),
        );
        object.insert(
          "end".into(),
          serde_json::json!({"line": end.line, "column": end.column}),
        );
      }
    }
    Value::Object(object) => {
      for value in object.values_mut() {
        relocate_value(value, files, sources)?;
      }
    }
    Value::Array(array) => {
      for value in array {
        relocate_value(value, files, sources)?;
      }
    }
    _ => {}
  }
  Ok(())
}

pub(super) fn relocate(entry: &mut Entry, current: Snapshot) -> Result<(), &'static str> {
  let proof = entry
    .layout
    .as_ref()
    .ok_or("no compiler-attested eligibility proof")?;
  if current.len() != entry.snapshot.len() {
    return Err("input membership changed");
  }
  let mut sources = BTreeMap::new();
  let mut replacements = BTreeMap::new();
  let mut size = 0;
  for (path, old) in &entry.snapshot {
    let now = current.get(path).ok_or("input membership changed")?;
    if old.as_ref().map(|v| &v.digest) == now.as_ref().map(|v| &v.digest) {
      continue;
    }
    let before = proof.sources.get(path).ok_or("non-source input changed")?;
    let old = old.as_ref().ok_or("input appeared")?;
    let now = now.as_ref().ok_or("input disappeared")?;
    if now.stamp.len > MAX_SOURCE as u64 {
      return Err("source size");
    }
    let after = fs::read_to_string(path).map_err(|_| "unreadable source")?;
    size += after.len();
    if size > MAX_SOURCES {
      return Err("aggregate source size");
    }
    if source_digest(before) != old.digest || source_digest(&after) != now.digest {
      return Err("proof source checksum");
    }
    if tokens(before)? != tokens(&after)? {
      return Err("tokens or punctuation jointness changed");
    }
    sources.insert(
      path.clone(),
      (
        crate::cache::Source::layout(before.clone()),
        crate::cache::Source::layout(after.clone()),
      ),
    );
    replacements.insert(path.clone(), after);
  }
  if sources.is_empty() {
    return Err("no eligible source edit");
  }
  let mut responses = Vec::new();
  for response in &entry.responses {
    let mut value = decode(response.output.as_bytes()).ok_or("invalid cached output")?;
    let files = value["Ok"]["files"].clone();
    relocate_value(&mut value, &files, &sources)?;
    let mut moved = prepare(value).ok_or("invalid relocated output")?;
    moved.selected_identity = response.selected_identity.clone();
    responses.push(moved);
  }
  let provenance = entry
    .provenance
    .as_ref()
    .ok_or("missing compiler provenance")?;
  let mut ranges: Vec<_> = provenance.bodies.iter().map(|b| b.range.clone()).collect();
  for range in &mut ranges {
    relocate_value(range, &Value::Null, &sources)?;
  }
  // The store publication lock is held by the caller. A second complete
  // snapshot rejects edits/ABA writes during the proof, including other inputs.
  if entry.snapshot().as_ref() != Some(&current) {
    return Err("inputs changed during relocation");
  }
  for (body, range) in entry
    .provenance
    .as_mut()
    .unwrap()
    .bodies
    .iter_mut()
    .zip(ranges)
  {
    body.range = range;
  }
  entry.responses = responses;
  entry.layout.as_mut().unwrap().sources.extend(replacements);
  entry.snapshot = current;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn whitespace_proof_preserves_spelling_comments_and_jointness() {
    assert_eq!(
      tokens("fn f(x: i32) -> i32 { x + 1 }\n").unwrap(),
      tokens("\nfn f(x:  i32) -> i32 {\n\tx + 1\n}\n").unwrap()
    );
    assert_ne!(tokens("a > > b").unwrap(), tokens("a >> b").unwrap());
    assert_ne!(tokens("1 . 2").unwrap(), tokens("1.2").unwrap());
    assert_ne!(
      tokens("x /* text */ + y").unwrap(),
      tokens("x /* changed */ + y").unwrap()
    );
    assert_ne!(
      tokens("x // text\n + y").unwrap(),
      tokens("x // text + y").unwrap()
    );
    for source in [
      "line!()",
      "#[derive(Clone)] struct T;",
      "/// docs\nfn f() {}",
      "std::panic::Location::caller()",
      "r#Location",
      "extern crate core;",
      "x\r\ny",
    ] {
      assert!(tokens(source).is_err(), "{source}");
    }
  }

  #[test]
  fn unicode_ranges_relocate_through_shared_token_anchors() {
    let before = crate::cache::Source::layout("fn f() { café + 1 }".into());
    let after = crate::cache::Source::layout("\nfn f() {\n  café + 1\n}".into());
    let (start, end) = before
      .relocate(&after, CharPos { line: 0, column: 9 }, CharPos {
        line: 0,
        column: 13,
      })
      .unwrap();
    assert_eq!(
      (start.line, start.column, end.line, end.column),
      (2, 2, 2, 6)
    );
  }
}
