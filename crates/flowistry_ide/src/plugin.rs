use std::{
  borrow::Cow,
  env,
  path::PathBuf,
  process::{Command, exit},
  time::Instant,
};

use anyhow::Context;
use base64::Engine;
use clap::{Parser, Subcommand};
use flowistry::extensions::{
  ContextMode, EVAL_MODE, EvalMode, MutabilityMode, PointerMode,
};
use fluid_let::fluid_set;
use log::{debug, info};
use rustc_hir::BodyId;
use rustc_interface::interface::Result as RustcResult;
use rustc_middle::ty::TyCtxt;
use rustc_plugin::{CrateFilter, RustcPlugin, RustcPluginArgs, Utf8Path};
use rustc_span::ErrorGuaranteed;
use rustc_utils::{
  mir::borrowck_facts,
  source_map::{
    filename::Filename,
    find_bodies::find_enclosing_bodies,
    range::{CharPos, CharRange, ToSpan},
  },
  timer::elapsed,
};
use serde::{Deserialize, Serialize};

#[derive(Parser, Serialize, Deserialize)]
pub struct FlowistryPluginArgs {
  #[clap(long)]
  bench: Option<bool>,

  #[clap(long)]
  context_mode: Option<ContextMode>,
  #[clap(long)]
  mutability_mode: Option<MutabilityMode>,
  #[clap(long)]
  pointer_mode: Option<PointerMode>,

  #[clap(subcommand)]
  command: FlowistryCommand,
}

#[derive(Subcommand, Serialize, Deserialize)]
enum FlowistryCommand {
  FileFocus {
    file: String,
    pos_line: Option<usize>,
    pos_column: Option<usize>,
  },
  Spans {
    file: String,
  },

  Focus {
    file: String,
    pos_line: usize,
    pos_column: usize,
  },

  Decompose {
    file: String,
    pos: usize,
  },

  Playground {
    file: String,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
  },

  Preload,

  RustcVersion,
}

/// The plugin arguments of this `cargo flowistry` invocation, serialized as
/// `rustc_plugin` passes them to the driver, and the file they are about. `None` for
/// commands that do not run the driver on a crate.
pub fn replay_request() -> Option<(String, PathBuf)> {
  let args = FlowistryPluginArgs::try_parse_from(env::args().skip(1)).ok()?;
  use FlowistryCommand::*;
  let file = match &args.command {
    Spans { file }
    | FileFocus { file, .. }
    | Focus { file, .. }
    | Decompose { file, .. }
    | Playground { file, .. } => PathBuf::from(file),
    Preload | RustcVersion => return None,
  };
  Some((serde_json::to_string(&args).ok()?, file))
}

pub struct FlowistryPlugin;
impl RustcPlugin for FlowistryPlugin {
  type Args = FlowistryPluginArgs;

  fn driver_name(&self) -> Cow<'static, str> {
    "flowistry-driver".into()
  }

  fn version(&self) -> Cow<'static, str> {
    env!("CARGO_PKG_VERSION").into()
  }

  fn args(&self, target_dir: &Utf8Path) -> RustcPluginArgs<FlowistryPluginArgs> {
    let args = FlowistryPluginArgs::parse_from(env::args().skip(1));

    let cargo_path = env::var("CARGO_PATH").unwrap_or_else(|_| "cargo".to_string());

    use FlowistryCommand::*;
    match &args.command {
      Preload => {
        let mut cmd = Command::new(cargo_path);
        // Note: this command must share certain parameters with rustc_plugin so Cargo will not recompute
        // dependencies when actually running the driver, e.g. RUSTFLAGS.
        cmd
          .args(["check", "--all", "--all-features", "--target-dir"])
          .arg(target_dir);
        let exit_status = cmd.status().expect("could not run cargo");
        exit(exit_status.code().unwrap_or(-1));
      }
      RustcVersion => {
        let version_str = rustc_interface::util::rustc_version_str().unwrap_or("unknown");
        println!("{version_str}");
        exit(0);
      }
      _ => {}
    };

    let file = match &args.command {
      FileFocus { file, .. } => file,
      Spans { file, .. } => file,
      Focus { file, .. } => file,
      Decompose { file, .. } => file,
      Playground { file, .. } => file,
      _ => unreachable!(),
    };

    RustcPluginArgs {
      filter: CrateFilter::CrateContainingFile(PathBuf::from(file)),
      args,
    }
  }

  fn run(
    self,
    compiler_args: Vec<String>,
    plugin_args: FlowistryPluginArgs,
  ) -> RustcResult<()> {
    crate::replay::record(&compiler_args);
    let eval_mode = EvalMode {
      context_mode: plugin_args.context_mode.unwrap_or(ContextMode::SigOnly),
      mutability_mode: plugin_args
        .mutability_mode
        .unwrap_or(MutabilityMode::DistinguishMut),
      pointer_mode: plugin_args.pointer_mode.unwrap_or(PointerMode::Precise),
    };
    fluid_set!(EVAL_MODE, eval_mode);

    use FlowistryCommand::*;
    match plugin_args.command {
      FileFocus {
        file,
        pos_line,
        pos_column,
      } => postprocess(crate::file_focus::analyze(
        &compiler_args,
        file,
        pos_line
          .zip(pos_column)
          .map(|(line, column)| CharPos { line, column }),
      )),
      Spans { file, .. } => postprocess(crate::spans::spans(&compiler_args, file)),
      Playground {
        file,
        start_line,
        start_column,
        end_line,
        end_column,
        ..
      } => {
        let compute_target = || {
          crate::positions::Chars(CharRange {
            start: CharPos {
              line: start_line,
              column: start_column,
            },
            end: CharPos {
              line: end_line,
              column: end_column,
            },
            filename: Filename::intern(&file),
          })
        };
        postprocess(run(
          crate::playground::playground,
          compute_target,
          &compiler_args,
        ))
      }
      Focus {
        file,
        pos_line,
        pos_column,
        ..
      } => {
        let compute_target = || {
          let cpos = CharPos {
            line: pos_line,
            column: pos_column,
          };
          let range = CharRange {
            start: cpos,
            end: cpos,
            filename: Filename::intern(&file),
          };
          debug!("eyo WTF {range:?} {file}");
          crate::positions::Chars(range)
        };
        postprocess(run(crate::focus::focus, compute_target, &compiler_args))
      }
      Decompose {
        file: _file,
        pos: _pos,
        ..
      } => {
        cfg_if::cfg_if! {
          if #[cfg(feature = "decompose")] {
            let indices = GraphemeIndices::from_path(&_file).unwrap();
            let id =
              FunctionIdentifier::Range(ByteRange::from_char_range(_pos, _pos, &_file, &indices));
            postprocess(run(
              crate::decompose::decompose,
              id,
              &compiler_args,
            ))
          } else {
            panic!("Flowistry must be built with the decompose feature")
          }
        }
      }
      _ => unreachable!(),
    }
  }
}

fn postprocess<T: Serialize>(result: FlowistryResult<T>) -> RustcResult<()> {
  let result = match result {
    Ok(output) => Ok(output),
    Err(e) => match e {
      FlowistryError::BuildError => {
        #[allow(deprecated)]
        return Err(ErrorGuaranteed::unchecked_error_guaranteed());
      }
      e => Err(e),
    },
  };

  let serialize_timer = Instant::now();
  // serde_json writes token by token. Without a buffer every tiny write goes through
  // the compressor, which dominated the run time for large focus outputs.
  // Level 6 (zlib's default) compresses about twice as fast as level 9 (`best`); the
  // output is larger (e.g. 2.6 instead of 1.5 MB for a 195 MB JSON), but it only goes
  // through a local pipe.
  let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
  let mut writer = std::io::BufWriter::with_capacity(1 << 16, encoder);
  serde_json::to_writer(&mut writer, &result).unwrap();
  let buffer = writer
    .into_inner()
    .unwrap_or_else(|e| panic!("{}", e.error()))
    .finish()
    .unwrap();
  log::info!(
    "output: serialize and compress took {:.4}s",
    serialize_timer.elapsed().as_secs_f64()
  );
  print!(
    "{}",
    base64::engine::general_purpose::STANDARD.encode(buffer)
  );

  Ok(())
}

pub fn run_with_callbacks(
  args: &[String],
  callbacks: &mut (dyn rustc_driver::Callbacks + Send),
) -> FlowistryResult<()> {
  // The analyses ask rustc for the few bodies they need (see `after_expansion`), so
  // loading and saving the incremental state cost more than it saves: ~10% of the
  // instructions of a typical focus.
  let mut kept = Vec::with_capacity(args.len());
  let mut rest = args.iter();
  while let Some(arg) = rest.next() {
    if arg == "-C"
      && rest
        .clone()
        .next()
        .is_some_and(|next| next.starts_with("incremental="))
    {
      rest.next();
    } else if !arg.starts_with("-Cincremental=") {
      kept.push(arg.clone());
    }
  }
  let mut args = kept;
  args.extend(
    "-Z identify-regions -Z mir-opt-level=0 -A warnings -Z maximal-hir-to-mir-coverage"
      .split(' ')
      .map(|s| s.to_owned()),
  );

  rustc_driver::catch_fatal_errors(move || rustc_driver::run_compiler(&args, callbacks))
    .map_err(|_| FlowistryError::BuildError)
}

fn run<A: FlowistryAnalysis, T: ToSpan>(
  analysis: A,
  compute_target: impl FnOnce() -> T + Send,
  args: &[String],
) -> FlowistryResult<A::Output> {
  let mut callbacks = FlowistryCallbacks {
    analysis: Some(analysis),
    compute_target: Some(compute_target),
    output: None,
    rustc_start: Instant::now(),
    eval_mode: EVAL_MODE.copied(),
  };

  info!("Starting rustc analysis...");
  debug!("Eval mode: {:?}", callbacks.eval_mode);

  run_with_callbacks(args, &mut callbacks)?;

  callbacks
    .output
    .unwrap()
    .map_err(|e| FlowistryError::AnalysisError {
      error: e.to_string(),
    })
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum FlowistryError {
  BuildError,
  AnalysisError { error: String },
  FileNotFound,
}

pub type FlowistryResult<T> = Result<T, FlowistryError>;

pub trait FlowistryAnalysis: Sized + Send + Sync {
  type Output: Serialize + Send + Sync;
  fn analyze(&mut self, tcx: TyCtxt, id: BodyId) -> anyhow::Result<Self::Output>;
}

// Implement FlowistryAnalysis for all functions with a type signature that matches
// FlowistryAnalysis::analyze
impl<F, O> FlowistryAnalysis for F
where
  F: for<'tcx> Fn<(TyCtxt<'tcx>, BodyId), Output = anyhow::Result<O>> + Send + Sync,
  O: Serialize + Send + Sync,
{
  type Output = O;
  fn analyze(&mut self, tcx: TyCtxt, id: BodyId) -> anyhow::Result<Self::Output> {
    (self)(tcx, id)
  }
}

struct FlowistryCallbacks<A: FlowistryAnalysis, T: ToSpan, F: FnOnce() -> T> {
  analysis: Option<A>,
  compute_target: Option<F>,
  output: Option<anyhow::Result<A::Output>>,
  rustc_start: Instant,
  eval_mode: Option<EvalMode>,
}

impl<A: FlowistryAnalysis, T: ToSpan, F: FnOnce() -> T> rustc_driver::Callbacks
  for FlowistryCallbacks<A, T, F>
{
  fn config(&mut self, config: &mut rustc_interface::Config) {
    borrowck_facts::enable_mir_simplification();
    config.override_queries = Some(borrowck_facts::override_queries);
  }

  /// Runs the analysis as soon as the crate is expanded and resolved, instead of after
  /// rustc's whole-crate `analysis` pass. rustc's queries are demand-driven, so only
  /// the target body and what the analysis needs (in `Recurse` mode, its callees) are
  /// type-checked and borrow-checked, with borrowck facts collected for those only.
  /// Errors elsewhere in the crate therefore do not stop the analysis.
  fn after_expansion<'tcx>(
    &mut self,
    _compiler: &rustc_interface::interface::Compiler,
    tcx: TyCtxt<'tcx>,
  ) -> rustc_driver::Compilation {
    elapsed("rustc", self.rustc_start);
    fluid_set!(EVAL_MODE, self.eval_mode.unwrap_or_default());

    let mut analysis = self.analysis.take().unwrap();
    self.output = Some((|| {
      let target = (self.compute_target.take().unwrap())().to_span(tcx)?;
      debug!("target span: {target:?}");
      let mut bodies = find_enclosing_bodies(tcx, target);
      let body = bodies.next().context("Selection did not map to a body")?;
      // The whole-crate pass used to stop on errors before the analysis ran. Keep
      // the analysis away from bodies that do not type-check; the errors are
      // reported and fail the request as before.
      if let Some(guar) = tcx
        .typeck(tcx.hir_body_owner_def_id(body))
        .tainted_by_errors
      {
        return Err(anyhow::Error::msg(format!(
          "the selected function does not type-check: {guar:?}"
        )));
      }
      analysis.analyze(tcx, body)
    })());

    // Without errors, write the output and exit: tearing down the compiler (freeing
    // its arenas and source files) takes longer than analyzing most bodies. With
    // errors, the driver reports them and fails the request as before.
    if tcx.dcx().has_errors().is_none() {
      let output =
        self
          .output
          .take()
          .unwrap()
          .map_err(|e| FlowistryError::AnalysisError {
            error: e.to_string(),
          });
      if postprocess(output).is_ok() {
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        std::process::exit(0);
      }
    }

    rustc_driver::Compilation::Stop
  }
}
