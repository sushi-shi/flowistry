use std::time::Instant;

use anyhow::Result;
use flowistry::infoflow::{self, Direction};
use itertools::Itertools;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir::BodyId;
use rustc_middle::ty::TyCtxt;
use rustc_span::{BytePos, Span, SpanData};
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

/// A place of the body. Its ranges are indices into [`FocusOutput::ranges`].
#[derive(Debug, Serialize)]
pub struct PlaceInfo {
  pub range: u32,
  pub ranges: Vec<u32>,
  pub slice: Vec<u32>,
  pub direct_influence: Vec<u32>,
}

#[derive(Debug, Serialize)]
pub struct FocusOutput {
  /// The distinct ranges of the places. The slices of the places of a body mostly
  /// share their ranges, so each is sent once and the places refer to it by index.
  pub ranges: Vec<CharRange>,
  pub place_info: Vec<PlaceInfo>,
  pub containers: Vec<CharRange>,
}

/// Builds [`FocusOutput::ranges`].
#[derive(Default)]
struct RangeTable {
  ranges: Vec<CharRange>,
  indices: FxHashMap<CharRange, u32>,
}

impl RangeTable {
  fn index(&mut self, range: CharRange) -> u32 {
    *self.indices.entry(range).or_insert_with(|| {
      self.ranges.push(range);
      u32::try_from(self.ranges.len() - 1).unwrap()
    })
  }
}

pub fn focus(tcx: TyCtxt, body_id: BodyId) -> Result<FocusOutput> {
  let def_id = tcx.hir_body_owner_def_id(body_id);
  let body_with_facts = get_body_with_borrowck_facts(tcx, def_id);
  let body = &body_with_facts.body;
  let results = &infoflow::compute_flow(tcx, body_id, body_with_facts);

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
    .map(|(k, vs)| (k, vs.concat()))
    .collect::<Vec<_>>();

  let targets = grouped_spans
    .iter()
    .map(|(_, target)| target.clone())
    .collect();

  let relevant = {
    block_timer!("focus: dependency spans");
    infoflow::compute_dependency_spans(results, targets, Direction::Both, &spanner)
  };

  let direct = {
    block_timer!("focus: direct influence");
    direct_influence::DirectInfluence::build(body, &results.analysis.place_info)
  };

  let slices_timer = Instant::now();
  // The same spans (and direct-influence locations) recur across the places of a body,
  // so convert each once instead of once per place.
  let mut table = RangeTable::default();
  let mut range_cache: FxHashMap<Span, Vec<u32>> = FxHashMap::default();
  let mut location_spans: FxHashMap<LocationOrArg, Vec<(SpanData, Span)>> =
    FxHashMap::default();
  let mut to_ranges = |table: &mut RangeTable, spans: &[Span]| -> Vec<u32> {
    let mut ranges = Vec::new();
    for span in spans {
      ranges.extend_from_slice(range_cache.entry(*span).or_insert_with(|| {
        span
          .trim_leading_whitespace(source_map)
          .into_iter()
          .flatten()
          .filter_map(|span| crate::positions::char_range(span, source_map).ok())
          .map(|range| table.index(range))
          .collect()
      }));
    }
    ranges
  };
  let mut slices = Vec::with_capacity(grouped_spans.len());
  for ((mir_span, targets), slice) in grouped_spans.iter().zip(relevant) {
    log::debug!("Slice for {mir_span:?} is {slice:#?}");

    // The slice's spans by start, with the largest end so far: a span is in the slice
    // if and only if a slice span starting at or before it ends at or after it.
    let mut starts = slice
      .iter()
      .map(|span| {
        let data = span.data_untracked();
        (data.lo, data.hi)
      })
      .collect::<Vec<_>>();
    starts.sort_unstable();
    let ends = starts
      .iter()
      .scan(BytePos(0), |end, (_, hi)| {
        *end = (*end).max(*hi);
        Some(*end)
      })
      .collect::<Vec<_>>();
    let in_slice = |data: &SpanData| {
      let before = starts.partition_point(|(lo, _)| *lo <= data.lo);
      before > 0 && ends[before - 1] >= data.hi
    };

    // `targets` has an entry per location of each place, and the rows of places
    // overlap: visit each influencing location once, and report each span once.
    let mut seen = FxHashSet::default();
    let mut direct_influence = Vec::new();
    for location in direct
      .lookup(targets.iter().map(|(target, _)| *target))
      .iter()
    {
      let spans = location_spans.entry(*location).or_insert_with(|| {
        spanner
          .location_to_spans(*location, body, EnclosingHirSpans::None)
          .into_iter()
          .map(|span| (span.data_untracked(), span))
          .collect()
      });
      for (data, span) in spans.iter() {
        if in_slice(data) && seen.insert(*span) {
          direct_influence.push(*span);
        }
      }
    }

    let Ok(range) = crate::positions::char_range(mir_span.span(), source_map) else {
      continue;
    };
    slices.push(PlaceInfo {
      range: table.index(range),
      ranges: to_ranges(&mut table, &[mir_span.span()]),
      slice: to_ranges(&mut table, &slice),
      direct_influence: to_ranges(&mut table, &direct_influence),
    });
  }
  log::info!(
    "focus: slice ranges took {:.4}s",
    slices_timer.elapsed().as_secs_f64()
  );

  let body_range = crate::positions::char_range(spanner.body_span, source_map)?;
  let ret_range = crate::positions::char_range(spanner.ret_span, source_map)?;
  let mut containers = vec![body_range, ret_range];

  let hir_body = tcx.hir_body(body_id);
  let arg_span = hir_body
    .params
    .iter()
    .map(|param| param.span)
    .reduce(|s1, s2| s1.to(s2));
  if let Some(sp) = arg_span {
    containers.push(crate::positions::char_range(sp, source_map)?);
  }

  Ok(FocusOutput {
    ranges: table.ranges,
    place_info: slices,
    containers,
  })
}
