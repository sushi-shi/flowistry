//! Persistent, compiler-validated focus results. No rustc identity is serialized.
use std::{cell::Cell, fs, hash::Hash, path::PathBuf, rc::Rc, sync::OnceLock};

use flowistry::{
  extensions::ContextMode, infoflow::AnalysisSession,
  mir::borrowck::body_with_borrowck_facts as get_body_with_borrowck_facts,
};
use rustc_data_structures::{
  fingerprint::Fingerprint,
  stable_hasher::{HashStable, StableHasher},
};
use rustc_hir::{self as hir, BodyId, HirId, OwnerNode};
use rustc_middle::ty::{self, TyCtxt, TypeVisitable, TypeVisitor};
use rustc_span::Span;
use rustc_utils::source_map::range::{CharPos, CharRange};
use serde::{Deserialize, Serialize};

use crate::focus::{FocusOutput, PlaceInfo};

const SCHEMA: u32 = 2;
const MAX_ENTRY: u64 = 32 * 1024 * 1024;

struct Regions(Vec<String>);
impl<'tcx> TypeVisitor<TyCtxt<'tcx>> for Regions {
  fn visit_region(&mut self, region: ty::Region<'tcx>) {
    self.0.push(format!("{region:?}"));
  }
}

pub struct FocusCache {
  directory: Option<PathBuf>,
  context: String,
  refresh: bool,
  pub hits: Cell<usize>,
  pub misses: Cell<usize>,
}

fn fingerprint(f: impl FnOnce(&mut StableHasher)) -> String {
  let mut h = StableHasher::new();
  f(&mut h);
  let value: Fingerprint = h.finish();
  format!("{value:?}")
}

fn engine() -> Option<&'static str> {
  static ENGINE: OnceLock<Option<String>> = OnceLock::new();
  ENGINE
    .get_or_init(|| {
      let data = fs::read(std::env::current_exe().ok()?).ok()?;
      Some(fingerprint(|h| data.hash(h)))
    })
    .as_deref()
}

/// Hash the declaration environment, including expanded types, constants,
/// attributes, imported crates and compiler configuration. Ordinary function
/// implementations are hashed separately via the resolved dependency closure.
fn context(tcx: TyCtxt<'_>) -> String {
  tcx.with_stable_hashing_context(|mut hcx| {
    fingerprint(|h| {
      hcx.while_hashing_spans(false, |hcx| {
        SCHEMA.hash(h);
        engine().hash(h);
        tcx.sess.opts.dep_tracking_hash(true).hash_stable(hcx, h);
        tcx
          .resolutions(())
          .visibilities_for_hashing
          .hash_stable(hcx, h);
        format!("{:?}", tcx.sess.target).hash(h);
        let mut cfg = tcx
          .sess
          .config
          .iter()
          .map(|(k, v)| (k.to_string(), v.map(|v| v.to_string())))
          .collect::<Vec<_>>();
        cfg.sort();
        cfg.hash(h);
        for &krate in tcx.crates(()) {
          tcx.stable_crate_id(krate).hash_stable(hcx, h);
          tcx.crate_hash(krate).hash_stable(hcx, h);
        }
        let mut owners = tcx.hir_crate_items(()).owners().collect::<Vec<_>>();
        owners.sort_by_cached_key(|owner| {
          format!("{:?}", tcx.def_path_hash(owner.def_id.to_def_id()))
        });
        for owner in owners {
          let node = tcx.hir_owner_node(owner);
          let dummy = BodyId {
            hir_id: HirId::make_owner(owner.def_id),
          };
          // Body IDs contain allocation-order indices. Normalize those while
          // retaining the complete declaration, so edits to unrelated ordinary
          // implementations do not invalidate every function.
          match node {
            OwnerNode::Item(item) => {
              let mut item = *item;
              if let hir::ItemKind::Fn { ref mut body, .. } = item.kind {
                *body = dummy;
              }
              item.hash_stable(hcx, h);
            }
            OwnerNode::ImplItem(item) => {
              let mut item = *item;
              if let hir::ImplItemKind::Fn(_, ref mut body) = item.kind {
                *body = dummy;
              }
              item.hash_stable(hcx, h);
            }
            OwnerNode::TraitItem(item) => {
              let mut item = *item;
              if let hir::TraitItemKind::Fn(_, hir::TraitFn::Provided(ref mut body)) =
                item.kind
              {
                *body = dummy;
              }
              item.hash_stable(hcx, h);
            }
            _ => node.hash_stable(hcx, h),
          }
          tcx
            .hir_attrs(HirId::make_owner(owner.def_id))
            .hash_stable(hcx, h);
          // Constants and opaque return types can influence callers without an
          // ordinary MIR call edge, so their defining bodies are global inputs.
          let opaque_return = node.fn_sig().is_some()
            && tcx
              .fn_sig(owner.def_id)
              .instantiate_identity()
              .skip_binder()
              .output()
              .walk()
              .any(|arg| {
                matches!(
                  arg.as_type().map(|t| t.kind()),
                  Some(ty::Alias(ty::AliasTy {
                    kind: ty::Opaque { .. },
                    ..
                  }))
                )
              });
          if opaque_return
            || (node.fn_sig().is_some() && tcx.is_const_fn(owner.def_id.to_def_id()))
          {
            for body in tcx.hir_owner_nodes(owner).bodies.values() {
              body.hash_stable(hcx, h);
            }
          }
        }
        for def in tcx.hir_body_owners() {
          if !tcx.hir_body_owner_kind(def).is_fn_or_closure() {
            tcx.hir_body_owned_by(def).hash_stable(hcx, h);
          }
        }
      });
    })
  })
}

impl FocusCache {
  pub fn new(tcx: TyCtxt<'_>) -> Self {
    let mode = std::env::var("FLOWISTRY_CACHE").unwrap_or_default();
    let directory = if mode == "off" || engine().is_none() {
      None
    } else {
      std::env::var_os("FLOWISTRY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
          std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
              std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
            })
            .map(|root| root.join("flowistry"))
        })
        .map(|root| root.join("focus-v1"))
    };
    Self {
      context: if directory.is_some() {
        context(tcx)
      } else {
        String::new()
      },
      directory,
      refresh: mode == "refresh",
      hits: Cell::new(0),
      misses: Cell::new(0),
    }
  }

  fn key<'tcx>(
    &self,
    tcx: TyCtxt<'tcx>,
    id: BodyId,
    session: &AnalysisSession<'tcx>,
    source: &Source,
  ) -> String {
    let root = tcx.hir_body_owner_def_id(id);
    let mut dependencies = if session.mode().context_mode == ContextMode::Recurse {
      session.dependencies(root)
    } else {
      vec![root]
    };
    dependencies
      .sort_by_cached_key(|def| format!("{:?}", tcx.def_path_hash(def.to_def_id())));
    tcx.with_stable_hashing_context(|mut hcx| {
      fingerprint(|h| {
        self.context.hash(h);
        session.mode().hash(h);
        source
          .tokens
          .iter()
          .map(|&(lo, hi)| &source.text[lo .. hi])
          .collect::<Vec<_>>()
          .hash(h);
        hcx.while_hashing_spans(false, |hcx| {
          for def in dependencies {
            def.hash_stable(hcx, h);
            let facts = get_body_with_borrowck_facts(tcx, def);
            let mut regions = Regions(Vec::new());
            facts.body.visit_with(&mut regions);
            regions.0.hash(h);
            tcx
              .erase_and_anonymize_regions(facts.body.clone())
              .hash_stable(hcx, h);
            // The alias model consumes these compiler-produced region relations.
            format!("{:?}", facts.input_facts).hash(h);
            // HIR mapping and nested closure bodies also affect source output.
            let owner = tcx.local_def_id_to_hir_id(def).owner;
            for body in tcx.hir_owner_nodes(owner).bodies.values() {
              body.hash_stable(hcx, h);
            }
          }
        });
      })
    })
  }

  pub fn focus<'tcx>(
    &self,
    tcx: TyCtxt<'tcx>,
    id: BodyId,
    session: Rc<AnalysisSession<'tcx>>,
  ) -> anyhow::Result<FocusOutput> {
    let span = tcx.hir_span_with_body(tcx.hir_body_owner(id));
    let prepared = self.directory.as_ref().and_then(|directory| {
      let source = Source::new(tcx, span)?;
      let key = self.key(tcx, id, &session, &source);
      log::debug!("Focus cache key: {key}, context: {}", self.context);
      Some((directory.join(format!("{key}.json")), key, source))
    });
    if let Some((path, key, source)) = &prepared {
      if !self.refresh {
        let cached = (|| {
          if fs::metadata(path).ok()?.len() > MAX_ENTRY {
            return None;
          }
          let entry: Entry = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
          if entry.schema != SCHEMA || entry.key != *key {
            return None;
          }
          entry.restore(source)
        })();
        if let Some(output) = cached {
          self.hits.set(self.hits.get() + 1);
          log::info!(
            "Focus cache hit: {}",
            tcx.def_path_str(tcx.hir_body_owner_def_id(id))
          );
          return Ok(output);
        }
      }
    }
    self.misses.set(self.misses.get() + 1);
    log::info!(
      "Focus cache miss: {}",
      tcx.def_path_str(tcx.hir_body_owner_def_id(id))
    );
    let output = crate::focus::focus_with_session(&session, id)?;
    if let Some((path, key, source)) = prepared {
      if let Some(entry) = Entry::capture(key, &source, &output) {
        // Cache failures must never turn successful analysis into an error.
        if let Err(error) = store(&path, &entry) {
          log::debug!("Focus cache write skipped: {error}");
        }
      } else {
        log::debug!("Focus cache cannot relocate all output ranges");
      }
    }
    Ok(output)
  }
}

struct Source {
  text: String,
  tokens: Vec<(usize, usize)>,
  range: CharRange,
}
impl Source {
  fn new(tcx: TyCtxt<'_>, span: Span) -> Option<Self> {
    if span.from_expansion() {
      return None;
    }
    let text = tcx.sess.source_map().span_to_snippet(span).ok()?;
    let range = crate::positions::char_range(span, tcx.sess.source_map()).ok()?;
    let mut offset = 0;
    let tokens = rustc_lexer::tokenize(&text, rustc_lexer::FrontmatterAllowed::No)
      .filter_map(|token| {
        let start = offset;
        offset += token.len as usize;
        (!matches!(token.kind, rustc_lexer::TokenKind::Whitespace))
          .then_some((start, offset))
      })
      .collect();
    Some(Self {
      text,
      tokens,
      range,
    })
  }
  fn byte(&self, pos: CharPos) -> Option<usize> {
    let row = pos.line.checked_sub(self.range.start.line)?;
    let column = if row == 0 {
      pos.column.checked_sub(self.range.start.column)?
    } else {
      pos.column
    };
    let mut start = 0;
    for _ in 0 .. row {
      start += self.text.get(start ..)?.find('\n')? + 1;
    }
    let line = self.text.get(start ..)?.split('\n').next()?;
    let byte = line
      .char_indices()
      .map(|(i, _)| i)
      .chain(std::iter::once(line.len()))
      .nth(column)?;
    Some(start + byte)
  }
  fn position(&self, byte: usize) -> Option<CharPos> {
    let prefix = self.text.get(.. byte)?;
    let line = prefix.bytes().filter(|&c| c == b'\n').count();
    let column = prefix.rsplit('\n').next()?.chars().count();
    Some(CharPos {
      line: self.range.start.line + line,
      column: column
        + if line == 0 {
          self.range.start.column
        } else {
          0
        },
    })
  }
  fn capture(&self, range: &CharRange) -> Option<PortableRange> {
    if range.filename != self.range.filename {
      return None;
    }
    let lo = self.byte(range.start)?;
    let hi = self.byte(range.end)?;
    let start = self
      .tokens
      .iter()
      .enumerate()
      .find_map(|(i, &(a, b))| (a <= lo && lo < b).then(|| (i, lo - a)))
      .or_else(|| {
        self
          .tokens
          .iter()
          .enumerate()
          .find_map(|(i, &(a, b))| (lo == b).then(|| (i, lo - a)))
      })?;
    let end = if lo == hi {
      start
    } else {
      self
        .tokens
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, &(a, b))| (a < hi && hi <= b).then(|| (i, hi - a)))
        .or_else(|| {
          self
            .tokens
            .iter()
            .enumerate()
            .find_map(|(i, &(a, _))| (hi == a).then_some((i, 0)))
        })?
    };
    Some(PortableRange { start, end })
  }
  fn restore(&self, range: &PortableRange) -> Option<CharRange> {
    let point = |(index, offset): (usize, usize)| {
      let &(a, b) = self.tokens.get(index)?;
      if offset > b - a {
        return None;
      }
      self.position(a + offset)
    };
    let start = point(range.start)?;
    let end = point(range.end)?;
    if (start.line, start.column) > (end.line, end.column) {
      return None;
    }
    Some(CharRange {
      start,
      end,
      filename: self.range.filename,
    })
  }
}

#[derive(Serialize, Deserialize)]
struct PortableRange {
  start: (usize, usize),
  end: (usize, usize),
}
#[derive(Serialize, Deserialize)]
struct PortablePlace {
  range: u32,
  ranges: Vec<u32>,
  slice: Vec<u32>,
  direct_influence: Vec<u32>,
  maybe_slice: Vec<u32>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
  schema: u32,
  key: String,
  integrity: String,
  ranges: Vec<PortableRange>,
  containers: Vec<PortableRange>,
  places: Vec<PortablePlace>,
}
impl Entry {
  fn checksum(&self) -> Option<String> {
    let bytes = serde_json::to_vec(&(
      self.schema,
      &self.key,
      &self.ranges,
      &self.containers,
      &self.places,
    ))
    .ok()?;
    Some(fingerprint(|h| bytes.hash(h)))
  }
  fn capture(key: String, source: &Source, output: &FocusOutput) -> Option<Self> {
    let list = |ranges: &[CharRange]| {
      ranges
        .iter()
        .map(|r| {
          source.capture(r).or_else(|| {
            log::debug!("Uncacheable range: {r:?}");
            None
          })
        })
        .collect::<Option<Vec<_>>>()
    };
    let mut entry = Self {
      schema: SCHEMA,
      key,
      integrity: String::new(),
      ranges: list(&output.ranges)?,
      containers: list(&output.containers)?,
      places: output
        .place_info
        .iter()
        .map(|p| {
          Some(PortablePlace {
            range: p.range,
            ranges: p.ranges.clone(),
            slice: p.slice.clone(),
            direct_influence: p.direct_influence.clone(),
            maybe_slice: p.maybe_slice.clone(),
          })
        })
        .collect::<Option<Vec<_>>>()?,
    };
    entry.integrity = entry.checksum()?;
    Some(entry)
  }
  fn restore(&self, source: &Source) -> Option<FocusOutput> {
    if self.checksum()? != self.integrity {
      return None;
    }
    let list = |ranges: &[PortableRange]| {
      ranges
        .iter()
        .map(|r| source.restore(r))
        .collect::<Option<Vec<_>>>()
    };
    for place in &self.places {
      if std::iter::once(&place.range)
        .chain(&place.ranges)
        .chain(&place.slice)
        .chain(&place.direct_influence)
        .chain(&place.maybe_slice)
        .any(|index| *index as usize >= self.ranges.len())
      {
        return None;
      }
    }
    Some(FocusOutput {
      ranges: list(&self.ranges)?,
      containers: list(&self.containers)?,
      place_info: self
        .places
        .iter()
        .map(|p| {
          Some(PlaceInfo {
            range: p.range,
            ranges: p.ranges.clone(),
            slice: p.slice.clone(),
            direct_influence: p.direct_influence.clone(),
            maybe_slice: p.maybe_slice.clone(),
          })
        })
        .collect::<Option<Vec<_>>>()?,
    })
  }
}

fn store(path: &std::path::Path, entry: &Entry) -> std::io::Result<()> {
  let data = serde_json::to_vec(entry)?;
  crate::result_store::Store::open(path.parent().unwrap().parent().unwrap())?.put(
    crate::result_store::Namespace::Focus,
    &entry.key,
    &data,
    None,
  )?;
  Ok(())
}
