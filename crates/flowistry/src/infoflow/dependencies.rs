use std::{cell::RefCell, collections::HashMap, iter};

use either::Either;
use indexical::ToIndex;
use log::{debug, trace};
use rustc_data_structures::fx::FxHashMap;
use rustc_index::IndexVec;
use rustc_middle::mir::*;
use rustc_span::{Span, SpanData, SyntaxContext};
use rustc_utils::{
  BodyExt, OperandExt, SpanExt, block_timer,
  mir::location_or_arg::{
    LocationOrArg,
    index::{LocationOrArgDomain, LocationOrArgIndex, LocationOrArgSet},
  },
  source_map::spanner::{EnclosingHirSpans, Spanner},
};

use super::{FlowDomain, FlowResults};
use crate::{
  extensions::ContextMode,
  infoflow::mutation::Mutation,
  mir::{bitset::IndexSetExt, placeinfo::PlaceInfo},
};

/// Which way to look for dependencies
#[derive(Clone, Copy, Debug)]
pub enum Direction {
  /// Things affects by the source
  Forward,

  /// Things that affect the source
  Backward,

  /// Both forward and backward
  Both,
}

#[derive(Debug, Clone)]
struct TargetDeps {
  all_forward: Vec<LocationOrArgSet>,
  /// The location of each sub-target, which its set in `all_forward` contains.
  pivots: Vec<LocationOrArg>,
}

impl TargetDeps {
  /// The target dependencies of each list of targets in `all_targets`.
  fn all<'tcx>(
    all_targets: &[Vec<(Place<'tcx>, LocationOrArg)>],
    results: &FlowResults<'_, 'tcx>,
  ) -> Vec<Self> {
    let place_info = &results.analysis.place_info;
    let location_domain = results.analysis.location_domain();

    // Every value reachable from a target is a sub-target at the target's location.
    let sub_targets = all_targets
      .iter()
      .enumerate()
      .flat_map(|(i, targets)| {
        targets.iter().flat_map(move |(place, location)| {
          place_info
            .reachable_values(*place, Mutability::Not)
            .iter()
            .map(move |reachable| (i, *reachable, *location))
        })
      })
      .collect::<Vec<_>>();

    // Visit the sub-targets location by location, so that each block's states are
    // computed once (see `FlowResults::state_at`).
    let state_location = |location: LocationOrArg| match location {
      LocationOrArg::Arg(..) => Location::START,
      LocationOrArg::Location(location) => location,
    };
    let mut order = (0 .. sub_targets.len()).collect::<Vec<_>>();
    order.sort_by_key(|i| state_location(sub_targets[*i].2));
    let mut forward = vec![None; sub_targets.len()];
    for i in order {
      let (_, place, location) = sub_targets[i];
      let state = results.state_at(state_location(location));

      let mut deps = LocationOrArgSet::new(location_domain);
      deps.insert_all();
      for conflict in place_info.norm_children(place_info.normalize(place)) {
        let conflict_deps = state.row_set(&conflict);
        trace!("place={place:?}, conflict={conflict:?}, deps={conflict_deps:?}");
        deps.intersect(conflict_deps);
      }
      deps.insert(location);
      forward[i] = Some(deps);
    }

    let mut all_target_deps = all_targets
      .iter()
      .map(|_| TargetDeps {
        all_forward: Vec::new(),
        pivots: Vec::new(),
      })
      .collect::<Vec<_>>();
    for ((i, _, location), deps) in iter::zip(sub_targets, forward) {
      all_target_deps[i].all_forward.push(deps.unwrap());
      all_target_deps[i].pivots.push(location);
    }
    all_target_deps
  }
}

/// The sub-targets of all targets, by the location of the sub-target.
///
/// A set of dependencies contains a sub-target's forward set only if it contains the
/// sub-target's location (the forward set always does), so only the sub-targets of
/// the locations in the set need the full inclusion test.
struct ForwardIndex {
  /// `(target, sub-target)` pairs by location.
  by_location: IndexVec<LocationOrArgIndex, Vec<(u32, u32)>>,
  /// The locations with sub-targets.
  locations: Vec<LocationOrArgIndex>,
}

impl ForwardIndex {
  fn new(all_target_deps: &[TargetDeps], domain: &LocationOrArgDomain) -> Self {
    let mut by_location = IndexVec::from_elem_n(Vec::new(), domain.len());
    for (i, target_deps) in all_target_deps.iter().enumerate() {
      for (j, pivot) in target_deps.pivots.iter().enumerate() {
        by_location[domain.index(pivot)].push((i as u32, j as u32));
      }
    }
    let locations = by_location
      .iter_enumerated()
      .filter(|(_, sub_targets)| !sub_targets.is_empty())
      .map(|(location, _)| location)
      .collect();
    ForwardIndex {
      by_location,
      locations,
    }
  }

  /// Calls `f` with the sub-targets whose location is in `deps`.
  fn candidates(&self, deps: &LocationOrArgSet, mut f: impl FnMut(u32, u32)) {
    let mut visit = |location: LocationOrArgIndex| {
      for (i, j) in &self.by_location[location] {
        f(*i, *j);
      }
    };
    // Either enumerate the set, or test the locations that have sub-targets.
    if deps.count() <= self.locations.len() {
      deps.indices().for_each(visit);
    } else {
      for location in &self.locations {
        if deps.contains(*location) {
          visit(*location);
        }
      }
    }
  }
}

pub fn deps<'a, 'tcx>(
  state: &'a FlowDomain<'tcx>,
  place_info: &'a PlaceInfo<'a, 'tcx>,
  place: Place<'tcx>,
) -> &'a LocationOrArgSet {
  state.row_set(&place_info.normalize(place))
}

/// Computes the dependencies of a place $p$ at a location $\ell$ in a given
/// direction.
///
/// * If the direction is backward, then the dependencies are locations that influence $p$.
/// * If the direction is forward, then the dependencies are locations that are influenced by $p$.
///
/// For efficiency reasons, this function actually takes a list of list of places at locations.
/// For example, if `all_targets = [[x@L1, y@L2], [z@L3]]` then the result would be
/// `[deps(x@L1) ∪ deps(y@L2), deps(z@L3)]`.
pub fn compute_dependencies<'tcx>(
  results: &FlowResults<'_, 'tcx>,
  all_targets: Vec<Vec<(Place<'tcx>, LocationOrArg)>>,
  direction: Direction,
) -> Vec<LocationOrArgSet> {
  compute_dependencies_inner(results, all_targets, direction, None)
}

fn compute_dependencies_inner<'tcx>(
  results: &FlowResults<'_, 'tcx>,
  all_targets: Vec<Vec<(Place<'tcx>, LocationOrArg)>>,
  direction: Direction,
  precomputed: Option<&[TargetDeps]>,
) -> Vec<LocationOrArgSet> {
  block_timer!("compute_dependencies");
  log::info!("Computing dependencies for {} targets", all_targets.len());
  debug!("all_targets={all_targets:#?}");

  let aliases = &results.analysis.place_info;
  let body = results.analysis.body;
  let location_domain = results.analysis.location_domain();

  let outputs = RefCell::new(
    all_targets
      .iter()
      .map(|_| LocationOrArgSet::new(location_domain))
      .collect::<Vec<_>>(),
  );

  let forward = || {
    let owned;
    let all_target_deps = match precomputed {
      Some(targets) => targets,
      None => {
        owned = TargetDeps::all(&all_targets, results);
        &owned
      }
    };
    log::info!(
      "sub-targets: {}",
      all_target_deps
        .iter()
        .map(|deps| deps.all_forward.len())
        .sum::<usize>()
    );
    debug!("all_target_deps={all_target_deps:#?}");

    for arg in body.args_iter() {
      let location = LocationOrArg::Arg(arg);
      for (target_deps, outputs) in iter::zip(all_target_deps, &mut *outputs.borrow_mut())
      {
        if target_deps
          .all_forward
          .iter()
          .any(|fwd| fwd.count() == 1 && fwd.contains(location))
        {
          outputs.insert(location);
        }
      }
    }

    let index = ForwardIndex::new(&all_target_deps, location_domain);
    results.for_each_state(|location, state| {
      // Converting a location to its index hashes it: do it once per location.
      let location_index = location.to_index(location_domain);
      let check = |place| {
        let deps = deps(state, aliases, place);
        let mut outputs = outputs.borrow_mut();
        index.candidates(deps, |i, j| {
          let outputs = &mut outputs[i as usize];
          if !outputs.contains(location_index)
            && deps.contains_all(&all_target_deps[i as usize].all_forward[j as usize])
          {
            outputs.insert(location_index);
          }
        });
      };

      match body.stmt_at(location) {
        Either::Right(Terminator {
          kind: TerminatorKind::SwitchInt { discr, .. },
          ..
        }) => {
          if let Some(place) = discr.as_place() {
            check(place);
          }
        }
        // In Recurse mode, the mutations of a terminator are those of the analysis
        // (e.g. from a callee summary), and it may read a target without writing
        // anything that depends on it (e.g. a call returning `()`).
        Either::Right(terminator)
          if results.analysis.place_info.mode().context_mode == ContextMode::Recurse =>
        {
          let effects = results.analysis.effects_at(terminator, location);
          for mutation in &effects.mutations {
            check(mutation.mutated);
          }
          if let Some(reads) = results.analysis.call_reads.borrow().get(&location) {
            for (target_deps, outputs) in
              iter::zip(all_target_deps, &mut *outputs.borrow_mut())
            {
              if target_deps
                .all_forward
                .iter()
                .any(|fwd| reads.contains_all(fwd))
              {
                outputs.insert(location);
              }
            }
          }
        }
        _ => {
          for mutations in results.analysis.modular_mutations(location).iter() {
            for Mutation { mutated, .. } in mutations {
              check(*mutated);
            }
          }
        }
      }
    });
  };

  let backward = || {
    let mut outputs = outputs.borrow_mut();
    let mut located = Vec::new();
    for (i, targets) in all_targets.iter().enumerate() {
      for (place, location) in targets {
        match location {
          LocationOrArg::Arg(..) => {
            outputs[i].insert(*location);
          }
          LocationOrArg::Location(location) => {
            // The place queries of `deps_for` happen in the order of the targets.
            results.analysis.prepare_deps_for(*place);
            located.push((*location, i, *place));
          }
        }
      }
    }
    // Then the states, block by block (see `FlowResults::state_at`).
    located.sort_by_key(|(location, ..)| *location);
    for (location, i, place) in located {
      let deps = results
        .analysis
        .deps_for(&results.state_at(location), place);
      outputs[i].union(&deps);
    }
  };

  match direction {
    Direction::Forward => forward(),
    Direction::Backward => backward(),
    Direction::Both => {
      forward();
      backward();
    }
  };

  outputs.into_inner()
}

/// Wraps [`compute_dependencies`] by translating each [`Location`] to a corresponding
/// source [`Span`] for the location.
pub fn compute_dependency_spans<'tcx>(
  results: &FlowResults<'_, 'tcx>,
  targets: Vec<Vec<(Place<'tcx>, LocationOrArg)>>,
  direction: Direction,
  spanner: &Spanner,
) -> Vec<Vec<Span>> {
  let body = results.analysis.body;

  let all_deps = compute_dependencies(results, targets, direction);
  debug!("all_deps={all_deps:?}");

  // The targets' dependencies share most locations: convert each location once.
  let mut location_spans = FxHashMap::default();
  all_deps
    .into_iter()
    .map(|deps| {
      let mut spans = Vec::new();
      for location in deps.iter() {
        spans.extend_from_slice(location_spans.entry(*location).or_insert_with(|| {
          spanner.location_to_spans(*location, body, EnclosingHirSpans::OuterOnly)
        }));
      }

      let merged_spans = merge_spans(spans);
      trace!("Spans: {merged_spans:?}");
      merged_spans
    })
    .collect::<Vec<_>>()
}

/// Merges the overlapping (or touching) spans of `spans`, with the same result as
/// [`SpanExt::merge_overlaps`] but in `O(n log n)` instead of `O(n²)`.
///
/// Once the spans are sorted by position, a span can only overlap the last merged
/// one. Each span is decoded once, and without tracking its parent: this runs after
/// the analysis, outside of any query, and decoding a span with tracking goes through
/// the incremental dependency graph.
pub fn merge_spans(spans: Vec<Span>) -> Vec<Span> {
  let mut spans = spans
    .into_iter()
    .map(|span| (span.data_untracked(), span))
    .collect::<Vec<_>>();
  spans.sort_by_key(|(data, _)| (data.lo, data.hi));

  let mut merged: Vec<(SpanData, Span)> = Vec::with_capacity(spans.len());
  for (data, span) in spans {
    // See the note in `SpanExt::subtract`.
    let span = span.with_ctxt(SyntaxContext::root());
    match merged.last_mut() {
      Some((last_data, last)) if data.lo <= last_data.hi && last_data.lo <= data.hi => {
        *last = span.to(*last);
        *last_data = last.data_untracked();
      }
      _ => merged.push((data, span)),
    }
  }
  merged.into_iter().map(|(_, span)| span).collect()
}

#[cfg(test)]
mod test {
  use rustc_span::BytePos;
  use rustc_utils::SpanExt;

  use super::*;

  #[test]
  fn merge_spans_matches_merge_overlaps() {
    rustc_span::create_default_session_globals_then(|| {
      // A small deterministic generator (xorshift64*).
      let mut state = 0x9e37_79b9_7f4a_7c15u64;
      let mut next = |n: u32| {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_f491_4f6c_dd1d) % u64::from(n)) as u32
      };
      for _ in 0 .. 2000 {
        let spans = (0 .. next(24))
          .map(|_| {
            let lo = next(200);
            let hi = lo + next(20);
            SpanData {
              lo: BytePos(lo),
              hi: BytePos(hi),
              ctxt: SyntaxContext::root(),
              parent: None,
            }
            .span()
          })
          .collect::<Vec<_>>();
        assert_eq!(
          merge_spans(spans.clone()),
          Span::merge_overlaps(spans.clone()),
          "{spans:?}"
        );
      }
    });
  }
}

/// Human-facing focus ranges: each target's forward and backward slice, with the
/// independent inputs of forward-only calls removed.
///
/// In `f(target, other)`, when the call is only in the forward slice, `other` is not
/// highlighted unless it depends on the target. Backward dependencies retain the
/// complete call: all its inputs may be needed to explain its result. Only
/// side-effect-free reads with trustworthy source spans are candidates for removal
/// (see [`super::simple_args::collect`]).
pub fn compute_focus_spans<'tcx>(
  results: &FlowResults<'_, 'tcx>,
  targets: Vec<Vec<(Place<'tcx>, LocationOrArg)>>,
  spanner: &Spanner,
) -> Vec<Vec<Span>> {
  block_timer!("compute_focus_spans");
  let body = results.analysis.body;
  let simple_args = super::simple_args::collect(
    results.analysis.tcx,
    results.analysis.def_id.expect_local(),
  );
  let target_deps = TargetDeps::all(&targets, results);
  let forward = compute_dependencies_inner(
    results,
    targets.clone(),
    Direction::Forward,
    Some(&target_deps),
  );
  let backward = compute_dependencies(results, targets, Direction::Backward);

  // Argument provenance and MIR-to-source conversion do not depend on the
  // selected variable. Compute them once per function, rather than per slice.
  let calls = body
    .all_locations()
    .filter_map(|location| {
      let Either::Right(Terminator {
        kind: TerminatorKind::Call { args, .. },
        ..
      }) = body.stmt_at(location)
      else {
        return None;
      };
      // state_at includes this instruction's effects. Call effects conservatively
      // mix the arguments; read their provenance BEFORE the call instead.
      let incoming = if location.statement_index > 0 {
        vec![Location {
          block: location.block,
          statement_index: location.statement_index - 1,
        }]
      } else {
        body.basic_blocks.predecessors()[location.block]
          .iter()
          .map(|block| body.terminator_loc(*block))
          .collect()
      };
      if incoming.is_empty() {
        return None;
      }
      let inputs = args
        .iter()
        .filter_map(|arg| {
          if !simple_args.contains(&arg.span) {
            return None;
          }
          let place = arg.node.as_place()?;
          let mut deps = LocationOrArgSet::new(results.analysis.location_domain());
          for previous in &incoming {
            deps.union(
              &results
                .analysis
                .deps_for(&results.state_at(*previous), place),
            );
          }
          Some((arg.span, deps))
        })
        .collect::<Vec<_>>();
      Some((LocationOrArg::Location(location), inputs))
    })
    .collect::<Vec<_>>();
  let mut span_cache = HashMap::new();
  for deps in forward.iter().chain(&backward) {
    for location in deps.iter() {
      span_cache.entry(*location).or_insert_with(|| {
        spanner.location_to_spans(*location, body, EnclosingHirSpans::OuterOnly)
      });
    }
  }

  forward
    .into_iter()
    .zip(backward)
    .zip(target_deps)
    .map(|((forward, mut backward), target)| {
      let excluded = calls
        .iter()
        .filter(|(location, _)| {
          forward.contains(*location) && !backward.contains(*location)
        })
        .flat_map(|(_, inputs)| inputs.iter())
        .filter(|(_, deps)| {
          !target
            .all_forward
            .iter()
            .any(|source| deps.contains_all(source))
        })
        .map(|(span, _)| *span)
        .collect::<Vec<_>>();
      backward.union(&forward);
      let spans = backward
        .iter()
        .flat_map(|location| span_cache[location].iter().copied())
        .collect::<Vec<_>>();
      merge_spans(spans)
        .into_iter()
        .flat_map(|span| span.subtract(excluded.clone()))
        .collect()
    })
    .collect()
}
