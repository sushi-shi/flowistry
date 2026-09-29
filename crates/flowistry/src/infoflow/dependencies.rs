use std::{cell::RefCell, iter};

use either::Either;
use log::{debug, trace};
use rustc_index::IndexVec;
use rustc_middle::mir::*;
use rustc_span::Span;
use rustc_utils::{
  OperandExt, SpanExt, block_timer,
  mir::location_or_arg::{
    LocationOrArg,
    index::{LocationOrArgDomain, LocationOrArgIndex, LocationOrArgSet},
  },
  source_map::spanner::{EnclosingHirSpans, Spanner},
};

use super::{FlowDomain, FlowResults};
use crate::{
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
    let all_target_deps = TargetDeps::all(&all_targets, results);
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
      for (target_deps, outputs) in
        iter::zip(&all_target_deps, &mut *outputs.borrow_mut())
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
      let check = |place| {
        let deps = deps(state, aliases, place);
        let mut outputs = outputs.borrow_mut();
        index.candidates(deps, |i, j| {
          let outputs = &mut outputs[i as usize];
          if !outputs.contains(location)
            && deps.contains_all(&all_target_deps[i as usize].all_forward[j as usize])
          {
            outputs.insert(location);
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

  all_deps
    .into_iter()
    .map(|deps| {
      let location_spans = deps
        .iter()
        .flat_map(|location| {
          spanner.location_to_spans(*location, body, EnclosingHirSpans::OuterOnly)
        })
        .collect::<Vec<_>>();

      let merged_spans = Span::merge_overlaps(location_spans);
      trace!("Spans: {merged_spans:?}");
      merged_spans
    })
    .collect::<Vec<_>>()
}
