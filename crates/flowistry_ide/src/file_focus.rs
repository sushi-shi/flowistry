//! Editor-independent batch analysis: reuse one compiler session for a file.
use flowistry::{
  extensions::{EVAL_MODE, EvalMode},
  infoflow::AnalysisSession,
};
use fluid_let::fluid_set;
use rustc_middle::ty::TyCtxt;
use rustc_utils::{
  SpanExt,
  mir::borrowck_facts,
  source_map::{
    filename::Filename,
    find_bodies::find_bodies,
    range::{CharPos, CharRange, ToSpan},
  },
};
use serde::Serialize;

use crate::{
  focus::FocusOutput,
  plugin::{FlowistryError, FlowistryResult},
};

#[derive(Serialize)]
pub struct BodyOutput {
  range: CharRange,
  focus: Option<Result<FocusOutput, String>>,
}

#[derive(Serialize)]
pub struct FileOutput {
  bodies: Vec<BodyOutput>,
}

struct Callbacks {
  filename: String,
  eval_mode: EvalMode,
  position: Option<CharPos>,
  output: Option<FlowistryResult<FileOutput>>,
}

impl rustc_driver::Callbacks for Callbacks {
  fn config(&mut self, config: &mut rustc_interface::Config) {
    borrowck_facts::enable_mir_simplification();
    config.override_queries = Some(borrowck_facts::override_queries);
  }

  fn after_expansion<'tcx>(
    &mut self,
    _compiler: &rustc_interface::interface::Compiler,
    tcx: TyCtxt<'tcx>,
  ) -> rustc_driver::Compilation {
    fluid_set!(EVAL_MODE, self.eval_mode);
    self.output = Some((|| {
      let source_map = tcx.sess.source_map();
      let filename = Filename::intern(&self.filename);
      let file = filename
        .find_source_file(source_map)
        .map_err(|_| FlowistryError::FileNotFound)?;
      let candidates = find_bodies(tcx);
      let selected = if let Some(position) = self.position {
        let target = CharRange {
          start: position,
          end: position,
          filename,
        }
        .to_span(tcx)
        .map_err(|error| FlowistryError::AnalysisError {
          error: error.to_string(),
        })?;
        candidates
          .iter()
          .filter(|(span, _)| span.contains(target))
          .min_by_key(|(span, _)| span.size())
          .map(|(_, id)| *id)
      } else {
        None
      };
      // The bodies of the file share their callee summaries.
      let session = AnalysisSession::new(tcx, self.eval_mode);
      let mut bodies = Vec::new();
      for (span, id) in candidates {
        if source_map.lookup_source_file(span.lo()).name != file.name {
          continue;
        }
        let Ok(range) = CharRange::from_span(span, source_map) else {
          continue;
        };
        bodies.push(BodyOutput {
          range,
          focus: if self.position.is_none() || selected == Some(id) {
            Some(if tcx.typeck(tcx.hir_body_owner_def_id(id)).tainted_by_errors.is_some() {
              Err("the selected function does not type-check".to_string())
            } else {
              crate::focus::focus_with_session(&session, id).map_err(|error| error.to_string())
            })
          } else {
            None
          },
        });
      }
      Ok(FileOutput { bodies })
    })());
    rustc_driver::Compilation::Stop
  }
}

pub fn analyze(
  args: &[String],
  filename: String,
  position: Option<CharPos>,
) -> FlowistryResult<FileOutput> {
  let mut callbacks = Callbacks {
    filename,
    position,
    eval_mode: EVAL_MODE.copied().unwrap_or_default(),
    output: None,
  };
  crate::plugin::run_with_callbacks(args, &mut callbacks)?;
  callbacks.output.unwrap()
}
