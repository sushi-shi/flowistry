use std::{collections::HashMap, rc::Rc, time::Instant};

use anyhow::Result;
use flowistry::{
  extensions::EvalMode,
  infoflow::{self, AnalysisSession},
};
use itertools::Itertools;
use rustc_hir::BodyId;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;
use rustc_utils::{
  SpanExt, block_timer,
  mir::{borrowck_facts::get_body_with_borrowck_facts, location_or_arg::LocationOrArg},
  source_map::{
    range::CharRange,
    spanner::{EnclosingHirSpans, Spanner},
  },
};
use serde::Serialize;

mod direct_influence;
#[cfg(test)]
mod tests;

#[derive(Debug, Serialize)]
pub struct PlaceInfo {
  pub range: CharRange,
  pub ranges: Vec<CharRange>,
  pub slice: Vec<CharRange>,
  pub direct_influence: Vec<CharRange>,
}

#[derive(Debug, Serialize)]
pub struct FocusOutput {
  pub place_info: Vec<PlaceInfo>,
  pub containers: Vec<CharRange>,
}

pub fn focus(tcx: TyCtxt, body_id: BodyId) -> Result<FocusOutput> {
  let session = AnalysisSession::new(tcx, EvalMode::from_ambient());
  focus_with_session(&session, body_id)
}

/// Like [`focus`], sharing the callee summaries of `session` (and in its mode).
pub(crate) fn focus_with_session<'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  body_id: BodyId,
) -> Result<FocusOutput> {
  let tcx = session.tcx();
  let def_id = tcx.hir_body_owner_def_id(body_id);
  let body_with_facts = get_body_with_borrowck_facts(tcx, def_id);
  let body = &body_with_facts.body;
  let results = &infoflow::compute_flow_with_session(session, body_id, body_with_facts);

  let source_map = tcx.sess.source_map();
  let spanner = {
    block_timer!("focus: span tree");
    Spanner::new(tcx, body_id, body)
  };

  let grouped_spans = spanner
    .mir_span_tree
    .iter()
    .map(|mir_span| {
      (
        mir_span.span,
        mir_span
          .locations
          .iter()
          .map(|location| (mir_span.place, *location))
          .collect::<Vec<_>>(),
      )
    })
    .into_group_map()
    .into_iter()
    .map(|(k, vs)| (k, vs.into_iter().flatten().unique().collect::<Vec<_>>()))
    .collect::<Vec<_>>();

  let targets = grouped_spans
    .iter()
    .map(|(_, target)| target.clone())
    .collect();

  let relevant = {
    block_timer!("focus: dependency spans");
    infoflow::compute_focus_spans(results, targets, &spanner)
  };

  let direct = {
    block_timer!("focus: direct influence");
    direct_influence::DirectInfluence::build(&results.analysis)
  };

  let slices_timer = Instant::now();
  // The same spans (and direct-influence locations) recur across the places of a body,
  // so convert each once instead of once per place.
  let mut range_cache: HashMap<Span, Vec<CharRange>> = HashMap::new();
  let mut location_spans: HashMap<LocationOrArg, Vec<Span>> = HashMap::new();
  let mut to_ranges = |spans: &[Span]| -> Vec<CharRange> {
    let mut ranges = Vec::new();
    for span in spans {
      ranges.extend_from_slice(range_cache.entry(*span).or_insert_with(|| {
        span
          .trim_leading_whitespace(source_map)
          .into_iter()
          .flatten()
          .filter_map(|span| CharRange::from_span(span, source_map).ok())
          .collect()
      }));
    }
    ranges
  };
  let mut slices = Vec::with_capacity(grouped_spans.len());
  for ((mir_span, targets), slice) in grouped_spans.iter().zip(relevant) {
    log::debug!("Slice for {mir_span:?} is {slice:#?}");

    let mut direct_influence = Vec::new();
    for location in targets
      .iter()
      .flat_map(|(target, _)| direct.lookup(*target))
    {
      let spans = location_spans.entry(location).or_insert_with(|| {
        spanner.location_to_spans(location, body, EnclosingHirSpans::None)
      });
      direct_influence.extend(
        spans
          .iter()
          .filter(|span| slice.iter().any(|slice_span| slice_span.contains(**span))),
      );
    }

    let Ok(range) = CharRange::from_span(mir_span.span(), source_map) else {
      continue;
    };
    slices.push(PlaceInfo {
      range,
      ranges: to_ranges(&[mir_span.span()]),
      slice: to_ranges(&slice),
      direct_influence: to_ranges(&direct_influence),
    });
  }
  log::info!(
    "focus: slice ranges took {:.4}s",
    slices_timer.elapsed().as_secs_f64()
  );

  let body_range = CharRange::from_span(spanner.body_span, source_map)?;
  let ret_range = CharRange::from_span(spanner.ret_span, source_map)?;
  let mut containers = vec![body_range, ret_range];

  let hir_body = tcx.hir_body(body_id);
  let arg_span = hir_body
    .params
    .iter()
    .map(|param| param.span)
    .reduce(|s1, s2| s1.to(s2));
  if let Some(sp) = arg_span {
    containers.push(CharRange::from_span(sp, source_map)?);
  }

  Ok(FocusOutput {
    place_info: slices,
    containers,
  })
}
