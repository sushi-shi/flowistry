//! One demand-driven pinned slice, with source identity across file boundaries.
use std::collections::BTreeMap;

use anyhow::{Context, Result};
use flowistry::{extensions::EvalMode, infoflow};
use rustc_hir::BodyId;
use rustc_index::Idx;
use rustc_middle::ty::TyCtxt;
use rustc_span::{FileName, RemapPathScopeComponents, Span};
use rustc_utils::{
  SpanExt,
  source_map::{
    range::{CharRange, ToSpan},
    spanner::Spanner,
  },
};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct PinnedOutput {
  schema: u32,
  bodies: Vec<BodyOutput>,
  files: BTreeMap<usize, Source>,
}

#[derive(Serialize)]
struct Source {
  path: String,
  // Check saved bytes before decorating a newly opened buffer. Filename IDs are
  // response-local, and neither matching paths nor matching line counts suffice.
  text: String,
}

#[derive(Serialize)]
struct BodyOutput {
  range: CharRange,
  containers: Vec<CharRange>,
  pre_slice: Vec<CharRange>,
  post_slice: Vec<CharRange>,
  maybe_pre_slice: Vec<CharRange>,
  maybe_post_slice: Vec<CharRange>,
  comments: Vec<CharRange>,
}

pub(crate) fn pinned(
  tcx: TyCtxt<'_>,
  root: BodyId,
  selection: CharRange,
) -> Result<PinnedOutput> {
  let mut selection = crate::positions::Chars(selection).to_span(tcx)?;
  for (label, initializer) in
    crate::focus::source_selection::field_initializers(tcx, root)
  {
    if label.contains(selection) {
      selection = initializer;
      break;
    }
  }
  let cache = crate::cache::FocusCache::new(tcx);
  let session = cache.session(tcx, EvalMode::from_ambient());
  let exact = infoflow::pinned_slice(&session, root, selection, false)
    .map_err(anyhow::Error::msg)?;
  let mut maybe = infoflow::pinned_slice(&session, root, selection, true)
    .map_err(anyhow::Error::msg)?;
  let mut bodies = Vec::new();
  let source_map = tcx.sess.source_map();
  let ranges = |spans: &[Span]| -> Vec<CharRange> {
    spans
      .iter()
      .flat_map(|span| span.trim_leading_whitespace(source_map).unwrap_or_default())
      .filter_map(|span| crate::positions::char_range(span, source_map).ok())
      .collect()
  };
  let subtract = |maybe: &[Span], exact: &[Span]| -> Vec<CharRange> {
    ranges(
      &maybe
        .iter()
        .flat_map(|span| span.subtract(exact.to_vec()))
        .collect::<Vec<_>>(),
    )
  };
  for exact in exact {
    let possible = maybe
      .iter()
      .position(|item| item.body == exact.body)
      .map(|i| maybe.remove(i));
    bodies.push((exact, possible));
  }
  for possible in maybe {
    bodies.push((
      infoflow::PinnedBody {
        body: possible.body,
        pre: vec![],
        post: vec![],
      },
      Some(possible),
    ));
  }
  let bodies = bodies
    .into_iter()
    .map(|(exact, possible)| -> Result<_> {
      let def = tcx.hir_body_owner_def_id(exact.body);
      let body = &flowistry::mir::borrowck::body_with_borrowck_facts(tcx, def).body;
      let spanner = Spanner::new(tcx, exact.body, body);
      let range = crate::positions::char_range(
        tcx.hir_span_with_body(tcx.hir_body_owner(exact.body)),
        source_map,
      )?;
      let mut containers = vec![spanner.body_span, spanner.ret_span];
      containers.extend(
        tcx
          .hir_body(exact.body)
          .params
          .iter()
          .map(|param| param.span),
      );
      let mut table = crate::focus::RangeTable::default();
      let (comments, _) =
        crate::focus::source_selection::collect(tcx, exact.body, &mut table);
      Ok(BodyOutput {
        range,
        containers: ranges(&containers),
        pre_slice: ranges(&exact.pre),
        post_slice: ranges(&exact.post),
        maybe_pre_slice: possible
          .as_ref()
          .map(|p| subtract(&p.pre, &exact.pre))
          .unwrap_or_default(),
        maybe_post_slice: possible
          .as_ref()
          .map(|p| subtract(&p.post, &exact.post))
          .unwrap_or_default(),
        comments: comments
          .into_iter()
          .map(|i| table.ranges[i as usize].clone())
          .collect(),
      })
    })
    .collect::<Result<Vec<_>>>()?;
  let mut files = BTreeMap::new();
  for body in &bodies {
    for range in std::iter::once(&body.range)
      .chain(&body.containers)
      .chain(&body.pre_slice)
      .chain(&body.post_slice)
      .chain(&body.maybe_pre_slice)
      .chain(&body.maybe_post_slice)
      .chain(&body.comments)
    {
      if files.contains_key(&range.filename.index()) {
        continue;
      }
      let file = range.filename.find_source_file(source_map)?;
      let FileName::Real(name) = &file.name else {
        anyhow::bail!("Unsupported source filename");
      };
      anyhow::ensure!(
        source_map.ensure_source_file_source_present(&file),
        "Missing pinned source"
      );
      // Cargo runs rustc from the workspace, which need not be the editor's
      // member-crate root. Resolve against the compiler's working directory,
      // and prefer the local path over a documentation/remapped filename.
      let path = name
        .local_path()
        .unwrap_or_else(|| name.path(RemapPathScopeComponents::DOCUMENTATION));
      let path = if path.is_absolute() {
        path.to_owned()
      } else {
        std::env::current_dir()?.join(path)
      };
      let path = path.canonicalize().unwrap_or(path);
      files.insert(range.filename.index(), Source {
        path: path
          .to_str()
          .context("Source path is not UTF-8")?
          .to_owned(),
        text: file
          .src
          .as_ref()
          .context("Missing pinned source text")?
          .to_string(),
      });
    }
  }
  Ok(PinnedOutput {
    schema: 1,
    bodies,
    files,
  })
}

#[cfg(test)]
mod tests {
  use flowistry::extensions::{ContextMode, EVAL_MODE};
  use rustc_span::BytePos;
  use rustc_utils::{
    mir::borrowck_facts, source_map::find_bodies::find_bodies, test_utils::CompileBuilder,
  };

  use super::*;

  #[test]
  fn pinned_calls_keep_directions_arguments_and_call_sites_separate() {
    let _ = env_logger::try_init();
    let source = r#"
fn leaf(value: i64, unrelated: i64) -> i64 {
 let used = value + 3;
 let other = unrelated + 7;
 used
}
fn goo(value: i64, unrelated: i64) -> i64 { leaf(value, unrelated) }
fn hello(x: i64, y: i64) -> i64 { let answer = goo(x, y); answer }
fn unrelated_caller(z: i64) -> i64 { goo(z, z) }
fn cycle_a(value: i64, depth: u8) -> i64 {
 if depth == 0 { value } else { cycle_b(value, depth - 1) }
}
fn cycle_b(value: i64, depth: u8) -> i64 { cycle_a(value, depth) }
fn cyclic(x: i64) -> i64 { cycle_a(x, 3) }
struct Pair { left: i64, right: i64 }
fn read_pair(pair: Pair) -> i64 {
 let used = pair.left + 3;
 let other = pair.right + 7;
 used
}
fn fields(x: i64, y: i64) -> i64 { read_pair(Pair { left: x, right: y }) }
fn construct(a: i64, b: i64) -> Pair { Pair { left: a, right: b } }
fn construction(x: i64, y: i64) -> i64 { let result = construct(x, y); result.left }
fn mutate(value: &mut i64, unrelated: &mut i64) { *value += 3; *unrelated += 7; }
fn mutation(x: i64, y: i64) -> i64 { let mut a = x; let mut b = y; mutate(&mut a, &mut b); a }
fn overwrite(mut value: i64, unrelated: i64) -> i64 { value = unrelated; value }
fn overwritten(x: i64, y: i64) -> i64 { overwrite(x, y) }
fn constant() -> i64 { 17 }
fn from_constant() -> i64 { let answer = constant(); answer }
fn controlled(x: bool) -> i64 { if x { constant() } else { 0 } }
struct Thing;
impl Thing { fn method(&self, value: i64, other: i64) -> i64 { leaf(value, other) } }
fn methods(x: i64, y: i64) -> i64 { Thing.method(x, y) }
fn closures(x: i64, y: i64) -> i64 {
 let closure = |value: i64, other: i64| leaf(value, other);
 closure(x, y)
}
fn labeled(key: i64, value: i64) -> Pair { Pair { left: key + 1, right: value + 2 } }
fn main() {}
"#;
    borrowck_facts::enable_mir_simplification();
    CompileBuilder::new(source).compile(|result| {
      let tcx = result.tcx;
      let bodies = find_bodies(tcx);
      for context_mode in [ContextMode::SigOnly, ContextMode::Recurse] {
        fluid_let::fluid_set!(EVAL_MODE, EvalMode {
          context_mode,
          ..EvalMode::default()
        });
        let analyze = |name: &str, needle: &str| {
          let (span, body) = bodies
            .iter()
            .find(|(_, id)| {
              tcx
                .opt_item_name(tcx.hir_body_owner_def_id(*id).to_def_id())
                .is_some_and(|item| item.as_str() == name)
            })
            .unwrap();
          let text = tcx.sess.source_map().span_to_snippet(*span).unwrap();
          let offset = text.find(needle).unwrap();
          let position = span.lo() + BytePos(offset as u32);
          pinned(
            tcx,
            *body,
            crate::positions::char_range(
              Span::with_root_ctxt(position, position),
              tcx.sess.source_map(),
            )
            .unwrap(),
          )
          .unwrap()
        };
        let text = |output: &PinnedOutput, function: &str, forward: bool| {
          output
            .bodies
            .iter()
            .filter(|body| {
              let text = tcx
                .sess
                .source_map()
                .span_to_snippet(body.range.to_span(tcx).unwrap())
                .unwrap();
              text.starts_with(&format!("fn {function}("))
            })
            .flat_map(|body| {
              if forward {
                &body.post_slice
              } else {
                &body.pre_slice
              }
            })
            .map(|range| {
              tcx
                .sess
                .source_map()
                .span_to_snippet(range.to_span(tcx).unwrap())
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
        };
        let out = analyze("hello", "x:");
        assert!(
          text(&out, "leaf", true).contains("value + 3"),
          "{context_mode:?}: {}",
          text(&out, "leaf", true)
        );
        assert!(!text(&out, "leaf", true).contains("unrelated + 7"));
        assert!(text(&out, "leaf", false).is_empty());
        assert!(text(&out, "unrelated_caller", true).is_empty());
        let out = analyze("hello", "answer }");
        assert!(
          text(&out, "leaf", false).contains("value + 3"),
          "{context_mode:?}: {}",
          text(&out, "leaf", false)
        );
        assert!(!text(&out, "leaf", false).contains("unrelated + 7"));
        let out = analyze("cyclic", "x:");
        assert!(text(&out, "cycle_a", true).contains("value"));
        assert!(text(&out, "cycle_b", true).contains("value"));
        let out = analyze("fields", "x:");
        assert!(text(&out, "read_pair", true).contains("pair.left + 3"));
        assert!(
          !text(&out, "read_pair", true).contains("pair.right + 7"),
          "{}",
          text(&out, "read_pair", true)
        );
        let out = analyze("construction", "x:");
        assert!(text(&out, "construct", true).contains("left: a"));
        assert!(!text(&out, "construct", true).contains("right: b"));
        let out = analyze("construction", "result.left");
        if context_mode == ContextMode::Recurse {
          // Backward source spans can include the whole aggregate expression,
          // but the unrelated callee input must not be followed into other calls.
          assert!(text(&out, "construct", false).contains("left: a"));
        }
        let out = analyze("mutation", "a }");
        assert!(
          text(&out, "mutate", false).contains("*value += 3"),
          "{context_mode:?}: {}",
          text(&out, "mutate", false)
        );
        if context_mode == ContextMode::Recurse {
          assert!(!text(&out, "mutate", false).contains("*unrelated += 7"));
        }
        let out = analyze("mutation", "x:");
        assert!(text(&out, "mutate", true).contains("*value += 3"));
        assert!(!text(&out, "mutate", true).contains("*unrelated += 7"));
        let out = analyze("overwritten", "x:");
        assert!(
          !text(&out, "overwrite", true).contains("value = unrelated"),
          "{}",
          text(&out, "overwrite", true)
        );
        let out = analyze("from_constant", "answer }");
        assert!(text(&out, "constant", false).contains("17"));
        let out = analyze("controlled", "x:");
        assert!(text(&out, "constant", true).contains("17"));
        for function in ["methods", "closures"] {
          let out = analyze(function, "x:");
          assert!(
            text(&out, "leaf", true).contains("value + 3"),
            "{function}/{context_mode:?}"
          );
          assert!(!text(&out, "leaf", true).contains("unrelated + 7"));
        }
        let out = analyze("labeled", "right:");
        assert!(text(&out, "labeled", false).contains("value + 2"));
        assert!(!text(&out, "labeled", false).contains("key + 1"));
      }
    });
  }
}
