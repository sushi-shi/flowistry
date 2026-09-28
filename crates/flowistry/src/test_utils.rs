//! Running rustc and Flowistry in tests.

#![allow(missing_docs)]

use std::{
  cell::RefCell,
  fs, io, panic,
  path::{Path, PathBuf},
  sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::Result;
use fluid_let::fluid_set;
use log::info;
use rustc_borrowck::consumers::BodyWithBorrowckFacts;
use rustc_data_structures::fx::FxHashSet as HashSet;
use rustc_hir::BodyId;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;
pub use rustc_utils::test_utils::{compare_ranges, fmt_ranges, parse_ranges};
use rustc_utils::{
  SpanExt,
  mir::borrowck_facts,
  source_map::{
    range::{ByteRange, CharPos, ToSpan},
    spanner::Spanner,
  },
  test_utils::{self, CompileBuilder},
};

use crate::{
  extensions::{ContextMode, EVAL_MODE, EvalMode, MutabilityMode, PointerMode},
  infoflow::{self, Direction},
};

pub fn compile_body_with_range(
  input: impl Into<String>,
  compute_target: impl FnOnce() -> ByteRange + Send,
  callback: impl for<'tcx> FnOnce(
    TyCtxt<'tcx>,
    BodyId,
    &'tcx BodyWithBorrowckFacts<'tcx>,
    ByteRange,
  ) + Send,
) {
  compile_body_with_range_and_args(input, compute_target, &[], callback)
}

/// Like [`compile_body_with_range`], with extra rustc arguments.
///
/// For example, pass `-Cincremental=<dir>` to compile with incremental compilation
/// enabled, as the IDE does.
pub fn compile_body_with_range_and_args(
  input: impl Into<String>,
  compute_target: impl FnOnce() -> ByteRange + Send,
  args: &[String],
  callback: impl for<'tcx> FnOnce(
    TyCtxt<'tcx>,
    BodyId,
    &'tcx BodyWithBorrowckFacts<'tcx>,
    ByteRange,
  ) + Send,
) {
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(input)
    .with_args(args.iter().cloned())
    .compile(|result| {
      let target = compute_target();
      let tcx = result.tcx;
      let (body_id, body_with_facts) = result.as_body_with_range(target);
      callback(tcx, body_id, body_with_facts, target)
    })
}

/// A fresh directory for incremental compilation, removed when dropped.
///
/// Flowistry runs under incremental compilation in the IDE, where some rustc queries
/// behave differently (e.g. trait queries on types with region variables panic).
/// Tests compile with [`IncrementalDir::args`] to exercise that configuration.
pub struct IncrementalDir(PathBuf);

impl IncrementalDir {
  /// Creates a new, unique, empty directory.
  pub fn new() -> Self {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir()
      .join(format!("flowistry-incremental-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    IncrementalDir(dir)
  }

  /// The rustc arguments enabling incremental compilation into this directory.
  pub fn args(&self) -> Vec<String> {
    vec![format!("-Cincremental={}", self.0.display())]
  }
}

impl Default for IncrementalDir {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for IncrementalDir {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

pub fn compile_body(
  input: impl Into<String>,
  callback: impl for<'tcx> FnOnce(TyCtxt<'tcx>, BodyId, &BodyWithBorrowckFacts<'tcx>) + Send,
) {
  borrowck_facts::enable_mir_simplification();
  test_utils::compile_body(input, callback)
}

/// Like [`compile_body`], with extra rustc arguments (see [`IncrementalDir`]).
pub fn compile_body_with_args(
  input: impl Into<String>,
  args: &[String],
  callback: impl for<'tcx> FnOnce(TyCtxt<'tcx>, BodyId, &BodyWithBorrowckFacts<'tcx>) + Send,
) {
  borrowck_facts::enable_mir_simplification();
  CompileBuilder::new(input)
    .with_args(args.iter().cloned())
    .compile(|result| {
      let (body_id, body_with_facts) = result.as_body();
      callback(result.tcx, body_id, body_with_facts)
    })
}

pub fn bless(
  tcx: TyCtxt,
  path: &Path,
  contents: String,
  actual: HashSet<ByteRange>,
) -> Result<()> {
  let mut delims = actual
    .into_iter()
    .flat_map(|byte_range| {
      let char_range = byte_range.as_char_range(tcx.sess.source_map());
      dbg!((byte_range, char_range));
      [("`[", char_range.start), ("]`", char_range.end)]
    })
    .collect::<Vec<_>>();
  delims.sort_by_key(|(_, i)| (i.line, i.column));

  let output = RefCell::new(String::new());
  let mut flush = |pos: CharPos| {
    while !delims.is_empty() && delims[0].1 == pos {
      let (delim, _) = delims.remove(0);
      output.borrow_mut().push_str(delim);
    }
  };

  let line_count = contents.lines().count();
  for (line, line_str) in contents.lines().enumerate() {
    for (column, chr) in line_str.chars().enumerate() {
      flush(CharPos { line, column });
      output.borrow_mut().push(chr);
    }
    flush(CharPos {
      line,
      column: line_str.chars().count(),
    });
    if line != line_count - 1 {
      output.borrow_mut().push('\n');
    }
  }

  fs::write(path.with_extension("txt.expected"), output.into_inner())?;

  Ok(())
}

/// Reads the [`EvalMode`] of a test fixture from its first line, e.g. `/* recurse */`.
pub fn eval_mode_from_header(input: &str) -> EvalMode {
  let header = input.lines().next().unwrap_or_default();
  let mut mode = EvalMode::default();
  if header.starts_with("/*") {
    if header.contains("recurse") {
      mode.context_mode = ContextMode::Recurse;
    }
    if header.contains("ignoremut") {
      mode.mutability_mode = MutabilityMode::IgnoreMut;
    }
    if header.contains("conservative") {
      mode.pointer_mode = PointerMode::Conservative;
    }
  }
  mode
}

/// Slices the places in `target` in the given direction, as the slicing fixtures do.
pub fn slice_spans<'tcx>(
  results: &infoflow::FlowResults<'_, 'tcx>,
  spanner: &Spanner<'tcx>,
  target: Span,
  direction: Direction,
) -> Vec<Span> {
  let places = spanner.span_to_places(target);
  let targets = places
    .iter()
    .map(|mir_span| {
      mir_span
        .locations
        .iter()
        .map(|location| (mir_span.place, *location))
        .collect::<Vec<_>>()
    })
    .collect();
  log::debug!("targets={targets:#?}");

  let deps = infoflow::compute_dependency_spans(results, targets, direction, spanner);

  Span::merge_overlaps(deps.into_iter().flatten().collect())
}

/// How a test chooses the [`EvalMode`] of an analysis.
#[derive(Clone, Copy, Debug)]
pub enum ModeSource {
  /// Pass the mode to [`infoflow::compute_flow_with_mode`], with [`EVAL_MODE`] unset.
  Explicit(EvalMode),
  /// Call [`infoflow::compute_flow`] with [`EVAL_MODE`] set to the given mode (or unset).
  Ambient(Option<EvalMode>),
}

/// Backward-slices the `` `(target)` `` of a fixture and returns the sorted byte offsets
/// of the slice, computing the flow as chosen by `source`.
pub fn backward_slice_offsets(
  input: &str,
  source: ModeSource,
  args: &[String],
) -> Vec<(usize, usize)> {
  let (input_clean, _) = parse_ranges(input, vec![("`(", ")`")]).unwrap();
  let output = std::sync::Mutex::new(Vec::new());
  compile_body_with_range_and_args(
    input_clean,
    || {
      let (_, input_ranges) = parse_ranges(input, vec![("`(", ")`")]).unwrap();
      input_ranges["`("][0]
    },
    args,
    |tcx, body_id, body_with_facts, target| {
      let target = target.to_span(tcx).unwrap();
      let results = match source {
        ModeSource::Explicit(mode) => {
          assert!(EVAL_MODE.copied().is_none());
          infoflow::compute_flow_with_mode(tcx, body_id, body_with_facts, mode)
        }
        ModeSource::Ambient(Some(mode)) => {
          fluid_set!(EVAL_MODE, mode);
          infoflow::compute_flow(tcx, body_id, body_with_facts)
        }
        ModeSource::Ambient(None) => {
          assert!(EVAL_MODE.copied().is_none());
          infoflow::compute_flow(tcx, body_id, body_with_facts)
        }
      };
      let spanner = Spanner::new(tcx, body_id, &body_with_facts.body);
      let mut offsets = slice_spans(&results, &spanner, target, Direction::Backward)
        .into_iter()
        .map(|span| {
          let range = ByteRange::from_span(span, tcx.sess.source_map()).unwrap();
          (range.start.0, range.end.0)
        })
        .collect::<Vec<_>>();
      offsets.sort();
      *output.lock().unwrap() = offsets;
    },
  );
  output.into_inner().unwrap()
}

pub fn test_command_output(
  path: &Path,
  expected: Option<&Path>,
  output_fn: impl for<'a, 'tcx> Fn(
    infoflow::FlowResults<'a, 'tcx>,
    Spanner<'tcx>,
    Span,
  ) -> Vec<Span>
  + Send
  + Sync,
) {
  test_command_output_with_args(path, expected, &[], output_fn)
}

/// Like [`test_command_output`], with extra rustc arguments (see [`IncrementalDir`]).
pub fn test_command_output_with_args(
  path: &Path,
  expected: Option<&Path>,
  args: &[String],
  output_fn: impl for<'a, 'tcx> Fn(
    infoflow::FlowResults<'a, 'tcx>,
    Spanner<'tcx>,
    Span,
  ) -> Vec<Span>
  + Send
  + Sync,
) {
  let inner = move || -> Result<()> {
    info!("Testing {}", path.file_name().unwrap().to_string_lossy());
    let input = String::from_utf8(fs::read(path)?)?;

    // We have to do a hacky thing where we call `parse_ranges` twice.
    // Once to clean up the input to pass to rustc to start the session.
    // A second time to get the `ByteRange`s, which *must* happen *within*
    // the session thread bc filenames are interned.
    let (input_clean, _) = parse_ranges(&input, vec![("`(", ")`")])?;
    compile_body_with_range_and_args(
      input_clean.clone(),
      || {
        let (_, input_ranges) = parse_ranges(&input, vec![("`(", ")`")]).unwrap();
        input_ranges["`("][0]
      },
      args,
      |tcx, body_id, body_with_facts, target: ByteRange| {
        let mode = eval_mode_from_header(&input);

        // The analysis receives the mode explicitly; the ambient mode is still set so
        // that code reading it (e.g. `utils::arg_mut_ptrs`) sees the same mode.
        fluid_set!(EVAL_MODE, &mode);

        let target = target.to_span(tcx).unwrap();
        let results =
          infoflow::compute_flow_with_mode(tcx, body_id, body_with_facts, mode);
        let spanner = Spanner::new(tcx, body_id, &body_with_facts.body);

        let actual = output_fn(results, spanner, target)
          .into_iter()
          .map(|span| ByteRange::from_span(span, tcx.sess.source_map()))
          .collect::<Result<HashSet<_>>>()
          .unwrap();

        match expected {
          Some(expected_path) => {
            let expected_file = fs::read_to_string(expected_path);
            match expected_file {
              Ok(file) => {
                let (_output_clean, output_ranges) =
                  parse_ranges(&file, vec![("`[", "]`")]).unwrap();

                let expected = match output_ranges.get("`[") {
                  Some(ranges) => ranges.clone().into_iter().collect::<HashSet<_>>(),
                  None => HashSet::default(),
                };

                compare_ranges(&expected, &actual, &input_clean);
              }
              Err(err) if matches!(err.kind(), io::ErrorKind::NotFound) => {
                println!("{}", fmt_ranges(&input_clean, &actual));
                panic!("Expected file not generated yet.");
              }
              err => {
                err.unwrap();
              }
            }
          }
          None => {
            bless(tcx, path, input_clean, actual).unwrap();
          }
        }
      },
    );

    Ok(())
  };

  inner().unwrap();
}

const BLESS: bool = option_env!("BLESS").is_some();
const ONLY: Option<&'static str> = option_env!("ONLY");
const EXIT: bool = option_env!("EXIT").is_some();

pub fn run_tests(
  dir: impl AsRef<Path>,
  test_fn: impl Fn(&Path, Option<&Path>) + std::panic::RefUnwindSafe,
) {
  run_tests_filtered(dir, |_| true, test_fn)
}

/// Like [`run_tests`], but only for the fixtures whose file name satisfies `filter`.
pub fn run_tests_filtered(
  dir: impl AsRef<Path>,
  filter: impl Fn(&str) -> bool,
  test_fn: impl Fn(&Path, Option<&Path>) + std::panic::RefUnwindSafe,
) {
  let main = || -> Result<()> {
    let test_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join(dir.as_ref());
    let tests = fs::read_dir(test_dir)?;
    let mut failed = false;
    for test in tests {
      let test = test?.path();
      if test.extension().unwrap() == "expected" {
        continue;
      }
      let test_name = test.file_name().unwrap().to_str().unwrap();
      if !filter(test_name) {
        continue;
      }
      if let Some(only) = ONLY {
        if !test_name.contains(only) {
          continue;
        }
      }
      let expected_path = test.with_extension("txt.expected");
      let expected = (!BLESS).then(|| expected_path.as_ref());

      let result = panic::catch_unwind(|| test_fn(&test, expected));
      if let Err(e) = result {
        if EXIT {
          panic!("{test_name}:\n{e:?}");
        } else {
          failed = true;
          eprintln!("\n\n{test_name}:\n{e:?}\n\n");
        }
      }
    }

    if failed {
      panic!("Tests failed.")
    }

    Ok(())
  };

  main().unwrap();
}
