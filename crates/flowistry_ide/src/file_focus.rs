//! Editor-independent batch analysis: reuse one compiler session for a file.
use flowistry::extensions::{EVAL_MODE, EvalMode};
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
  cached: Option<bool>,
}

#[derive(Serialize)]
pub struct FileOutput {
  bodies: Vec<BodyOutput>,
  cache: CacheStats,
}

#[derive(Serialize)]
struct CacheStats {
  hits: usize,
  misses: usize,
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
    let mut identities = Vec::new();
    self.output = Some((|| {
      let source_map = tcx.sess.source_map();
      let filename = Filename::intern(&self.filename);
      let file = filename
        .find_source_file(source_map)
        .map_err(|_| FlowistryError::FileNotFound)?;
      let candidates = find_bodies(tcx);
      let selected = if let Some(position) = self.position {
        let target = crate::positions::Chars(CharRange {
          start: position,
          end: position,
          filename,
        })
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
      let cache = crate::cache::FocusCache::new(tcx);
      let session = cache.session(tcx, self.eval_mode);
      let mut bodies = Vec::new();
      for (span, id) in candidates {
        if source_map.lookup_source_file(span.lo()).name != file.name {
          continue;
        }
        let Ok(range) = crate::positions::char_range(span, source_map) else {
          continue;
        };
        identities.push(crate::fast_cache::BodyIdentity::new(tcx, id, &range));
        let previous_hits = cache.hits.get();
        let focus = if self.position.is_none() || selected == Some(id) {
          Some(
            if tcx
              .typeck(tcx.hir_body_owner_def_id(id))
              .tainted_by_errors
              .is_some()
            {
              Err("the selected function does not type-check".to_string())
            } else {
              cache
                .focus(tcx, id, session.clone())
                .map_err(|error| error.to_string())
            },
          )
        } else {
          None
        };
        bodies.push(BodyOutput {
          range,
          cached: focus.as_ref().map(|_| cache.hits.get() > previous_hits),
          focus,
        });
      }
      Ok(FileOutput {
        bodies,
        cache: CacheStats {
          hits: cache.hits.get(),
          misses: cache.misses.get(),
        },
      })
    })());
    crate::fast_cache::record_inputs(tcx, identities);
    if tcx.dcx().has_errors().is_none() {
      if crate::plugin::postprocess(self.output.take().unwrap()).is_ok() {
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        std::process::exit(0);
      }
    }
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
