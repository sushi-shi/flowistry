use std::collections::HashMap;

use anyhow::Result;
use flowistry::infoflow;
use itertools::Itertools;
use rustc_hir::BodyId;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;
use rustc_utils::{
  SpanExt,
  mir::borrowck_facts::get_body_with_borrowck_facts,
  source_map::{
    range::CharRange,
    spanner::{EnclosingHirSpans, Spanner},
  },
};
use serde::Serialize;

mod direct_influence;
mod simple_args;

#[cfg(test)]
mod tests;

#[derive(Debug, Serialize)]
pub struct PlaceInfo {
  pub range: CharRange,
  pub ranges: Vec<CharRange>,
  pub slice: Vec<CharRange>,
  pub direct_influence: Vec<CharRange>,
  /// Code that may be relevant only if separately held shared handles to the
  /// same interior-mutable type (e.g. two `Rc<RefCell<T>>`) are one object.
  /// Disjoint from `slice`.
  /// Omitted when empty, keeping the protocol unchanged for other code.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub maybe_slice: Vec<CharRange>,
}

#[derive(Debug, Serialize)]
pub struct FocusOutput {
  pub place_info: Vec<PlaceInfo>,
  pub containers: Vec<CharRange>,
}

pub fn focus(tcx: TyCtxt, body_id: BodyId) -> Result<FocusOutput> {
  focus_with_session(tcx, body_id, infoflow::AnalysisSession::new(tcx))
}

pub(crate) fn focus_with_session<'tcx>(
  tcx: TyCtxt<'tcx>,
  body_id: BodyId,
  session: std::rc::Rc<infoflow::AnalysisSession<'tcx>>,
) -> Result<FocusOutput> {
  let def_id = tcx.hir_body_owner_def_id(body_id);
  let body_with_facts = get_body_with_borrowck_facts(tcx, def_id);
  let body = &body_with_facts.body;
  let results =
    &infoflow::compute_flow_with_session(session.clone(), tcx, body_id, body_with_facts);
  let shared_results =
    infoflow::compute_flow_with_shared_handles(session, tcx, body_id, body_with_facts);

  let source_map = tcx.sess.source_map();
  let spanner = Spanner::new(tcx, body_id, body);

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
    .collect::<Vec<_>>();

  let simple_args = simple_args::collect(tcx, body_id);
  let maybe_relevant = shared_results.as_ref().map(|shared| {
    infoflow::compute_focus_spans(shared, targets.clone(), &spanner, &simple_args)
  });
  let relevant = infoflow::compute_focus_spans(results, targets, &spanner, &simple_args);

  let direct = direct_influence::DirectInfluence::build(&results.analysis);

  let mut direct_spans = HashMap::new();
  let mut range_cache = HashMap::new();
  let mut to_ranges = |spans: Vec<Span>| {
    let mut output = Vec::new();
    for span in spans {
      let ranges = range_cache.entry(span).or_insert_with(|| {
        span
          .trim_leading_whitespace(source_map)
          .into_iter()
          .flatten()
          .filter_map(|span| CharRange::from_span(span, source_map).ok())
          .collect::<Vec<_>>()
      });
      output.extend_from_slice(ranges);
    }
    output
  };

  let slices = grouped_spans
    .iter()
    .zip(relevant)
    .enumerate()
    .filter_map(|(i, ((mir_span, targets), relevant))| {
      log::debug!("Slice for {mir_span:?} is {relevant:#?}");

      let direct_influence = targets
        .iter()
        .flat_map(|(target, _)| direct.lookup(*target))
        .flat_map(|location| {
          direct_spans
            .entry(location)
            .or_insert_with(|| {
              spanner.location_to_spans(location, body, EnclosingHirSpans::None)
            })
            .clone()
        })
        .filter(|span| relevant.iter().any(|slice_span| slice_span.contains(*span)))
        .collect::<Vec<_>>();

      let maybe_slice = maybe_relevant
        .as_ref()
        .map(|maybe| {
          maybe[i]
            .iter()
            .filter(|span| !relevant.iter().any(|exact| exact.contains(**span)))
            .copied()
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();
      let slice = relevant;

      log::debug!("{:#?}", to_ranges(slice.clone()));

      Some(PlaceInfo {
        range: CharRange::from_span(mir_span.span(), source_map).ok()?,
        ranges: to_ranges(vec![mir_span.span()]),
        slice: to_ranges(slice),
        direct_influence: to_ranges(direct_influence),
        maybe_slice: to_ranges(maybe_slice),
      })
    })
    .collect::<Vec<_>>();

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
