//! Source slices that follow a pinned value down compiler-resolved local calls.
//!
//! Call inputs are read before the call, outputs after it. Each direction stays
//! independent: following an affected argument never turns into a backward query
//! for the callee's other inputs. A body/seed worklist terminates recursive cycles
//! and unions only call sites reached from the selected root, not other callers.

use std::{collections::VecDeque, rc::Rc};

use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir::{BodyId, def_id::LocalDefId};
use rustc_middle::mir::{Location, Place, RETURN_PLACE, TerminatorKind};
use rustc_span::Span;
use rustc_utils::{
  SpanExt, mir::location_or_arg::LocationOrArg, source_map::spanner::Spanner,
};

use super::{
  AnalysisSession, Direction, FlowResults, build_place_info,
  callsite::{CallSite, CalleeAbi, EffectPath, Resolved, RowRole, Target},
  compute_dependencies, compute_flow_with_session, compute_flow_with_shared_handles,
  compute_focus_directions,
  dependencies::{TargetDeps, value_targets},
  merge_spans,
  recursive::resolve_callee,
};

type Seeds<'tcx> = Vec<(Place<'tcx>, LocationOrArg)>;

/// The two source directions in one body reached from a pin.
pub struct PinnedBody {
  /// Compiler-local identity of this function.
  pub body: BodyId,
  /// Sources that may affect the pinned value.
  pub pre: Vec<Span>,
  /// Uses that the pinned value may affect.
  pub post: Vec<Span>,
}

struct Boundary<'tcx> {
  abi: CalleeAbi,
  inputs: Vec<(Place<'tcx>, EffectPath)>,
  outputs: Vec<(Place<'tcx>, EffectPath)>,
  returns: Vec<Location>,
}

fn boundary<'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  def: LocalDefId,
) -> Boundary<'tcx> {
  let tcx = session.tcx();
  let facts = session.body(def);
  let body = &facts.body;
  let info = build_place_info(session, tcx.hir_body_owned_by(def).id(), facts);
  let abi = CalleeAbi::of_body(tcx, def.to_def_id(), body);
  let mut places = info.all_args().map(|(place, _)| place).collect::<Vec<_>>();
  places.extend(info.children(Place::from(RETURN_PLACE)).iter().copied());
  // Keep aggregate fields separate. A pointer and its pointee are different
  // values, so a dereference is not a reason to discard the pointer itself.
  let leaves = places.iter().copied().filter(|place| {
    !places.iter().any(|child| {
      child.local == place.local
        && child.projection.len() > place.projection.len()
        && child.projection.starts_with(place.projection)
        && !matches!(
          child.projection[place.projection.len()],
          rustc_middle::mir::ProjectionElem::Deref
        )
    })
  });
  let mut inputs = Vec::new();
  let mut outputs = Vec::new();
  for place in leaves {
    match abi.classify(info.normalize(place)) {
      RowRole::ArgDirect(path) => inputs.push((place, path)),
      RowRole::ArgPointee(path) => {
        inputs.push((place, path.clone()));
        outputs.push((place, path));
      }
      RowRole::Return(path) => outputs.push((place, path)),
      RowRole::Internal => {}
    }
  }
  let returns = body
    .basic_blocks
    .iter_enumerated()
    .filter_map(|(block, data)| {
      matches!(data.terminator().kind, TerminatorKind::Return)
        .then(|| body.terminator_loc(block))
    })
    .collect();
  Boundary {
    abi,
    inputs,
    outputs,
    returns,
  }
}

/// Compute both directions from one source selection. `shared` includes the
/// pessimistic shared-handle analysis so the editor can distinguish maybe ranges.
/// Only local resolved callees are followed; external and dynamic calls retain
/// their ordinary conservative effects in the caller.
pub fn pinned_slice<'tcx>(
  session: &Rc<AnalysisSession<'tcx>>,
  root: BodyId,
  selection: Span,
  shared: bool,
) -> Result<Vec<PinnedBody>, &'static str> {
  let tcx = session.tcx();
  let root_def = tcx.hir_body_owner_def_id(root);
  let spanner = Spanner::new(tcx, root, &session.body(root_def).body);
  let smallest = spanner
    .mir_span_tree
    .iter()
    .filter(|item| item.span.span().contains(selection))
    .map(|item| item.span.span().size())
    .min()
    .ok_or("The pinned token does not select a Rust value")?;
  let seeds = spanner
    .mir_span_tree
    .iter()
    .filter(|item| {
      item.span.span().contains(selection) && item.span.span().size() == smallest
    })
    .flat_map(|item| item.locations.iter().map(|at| (item.place, *at)))
    .collect::<Seeds<'tcx>>();
  let mut pending = VecDeque::from([
    (root_def, false, seeds.clone(), false),
    (root_def, true, seeds, false),
  ]);
  let mut executions = FxHashSet::default();
  let mut seen: FxHashMap<(LocalDefId, bool), FxHashSet<_>> = FxHashMap::default();
  let mut analyses: FxHashMap<LocalDefId, FlowResults<'tcx, 'tcx>> = FxHashMap::default();
  let mut boundaries = FxHashMap::default();
  let mut output: FxHashMap<LocalDefId, PinnedBody> = FxHashMap::default();
  while let Some((def, forward, seeds, execution)) = pending.pop_front() {
    let visited = seen.entry((def, forward)).or_default();
    let seeds = seeds
      .into_iter()
      .filter(|seed| visited.insert(*seed))
      .collect::<Seeds<'tcx>>();
    let new_execution = execution && executions.insert(def);
    if seeds.is_empty() && !new_execution {
      continue;
    }
    if analyses.len() >= 256 && !analyses.contains_key(&def) {
      return Err("Pinned slice exceeds 256 local functions; choose a narrower value");
    }
    let body_id = tcx.hir_body_owned_by(def).id();
    let results = analyses.entry(def).or_insert_with(|| {
      let facts = session.body(def);
      if shared {
        if let Some(results) = compute_flow_with_shared_handles(session, body_id, facts) {
          return results;
        }
      }
      compute_flow_with_session(session, body_id, facts)
    });
    let body = results.analysis.body;
    let spanner = Spanner::new(tcx, body_id, body);
    let inputs = (forward
      && def != root_def
      && seeds
        .iter()
        .all(|(_, at)| matches!(at, LocationOrArg::Arg(_))))
    .then(|| {
      super::summary::input_flow(
        session,
        def,
        seeds.iter().map(|(place, _)| *place).collect(),
        shared,
      )
    });
    let seeds = value_targets(results, vec![seeds], &spanner).pop().unwrap();
    let directions = compute_focus_directions(results, vec![seeds.clone()], &spanner);
    let entry = output.entry(def).or_insert_with(|| PinnedBody {
      body: body_id,
      pre: Vec::new(),
      post: Vec::new(),
    });
    if execution {
      entry.post.extend([spanner.body_span, spanner.ret_span]);
      entry
        .post
        .extend(tcx.hir_body(body_id).params.iter().map(|param| param.span));
    } else if forward {
      entry.post.extend(
        inputs
          .as_ref()
          .map(|input| input.spans(&spanner))
          .unwrap_or_else(|| directions.post.into_iter().next().unwrap()),
      );
    } else {
      entry.pre.extend(directions.pre.into_iter().next().unwrap());
    }
    let forward_targets = TargetDeps::all(&[seeds.clone()], results).pop().unwrap();
    let backward = compute_dependencies(results, vec![seeds], Direction::Backward)
      .pop()
      .unwrap();
    log::debug!(
      "pin {} forward={forward} backward={backward:?}",
      tcx.def_path_str(def)
    );

    for (block, data) in body.basic_blocks.iter_enumerated() {
      let call = &data.terminator().kind;
      let TerminatorKind::Call { func, .. } = call else {
        continue;
      };
      let at = body.terminator_loc(block);
      if !forward && !backward.contains(at) {
        continue;
      }
      let Ok(callee) = resolve_callee(tcx, def, func) else {
        continue;
      };
      let boundary = boundaries
        .entry(callee)
        .or_insert_with(|| boundary(session, callee));
      let Ok(site) = CallSite::new(
        tcx,
        def.to_def_id(),
        body,
        call,
        callee.to_def_id(),
        boundary.abi,
      ) else {
        continue;
      };
      // A value can affect whether a call happens without affecting any of its
      // argument values. Carry that execution dependence into the whole callee.
      let controls_call = forward
        && (execution
          || results
            .analysis
            .control_dependencies
            .dependent_on(block)
            .into_iter()
            .flat_map(|blocks| blocks.iter())
            .any(|guard| {
              let TerminatorKind::SwitchInt { discr, .. } =
                &body.basic_blocks[guard].terminator().kind
              else {
                return false;
              };
              let Some(place) = discr.place() else {
                return false;
              };
              let guard_at = body.terminator_loc(guard);
              inputs
                .as_ref()
                .map(|input| input.reaches_before(guard_at, place))
                .unwrap_or_else(|| {
                  forward_targets.reaches(
                    &results
                      .analysis
                      .deps_of_inputs(&results.state_before(guard_at), &[place]),
                  )
                })
            }));
      let before = results.state_before(at);
      let after = results.state_at(at);
      let mut next = Vec::new();
      for (place, path) in if forward {
        &boundary.inputs
      } else {
        &boundary.outputs
      } {
        let caller_place = match site.translate(path) {
          Resolved::Target(Target::Exact(place) | Target::Coarsened { place, .. }) => {
            place
          }
          Resolved::NoCallerState => continue,
        };
        if forward {
          let deps = results.analysis.deps_of_inputs(&before, &[caller_place]);
          if inputs
            .as_ref()
            .map(|input| input.reaches_before(at, caller_place))
            .unwrap_or_else(|| forward_targets.reaches(&deps))
          {
            next.push((*place, LocationOrArg::Arg(place.local)));
          }
        } else {
          let deps = results.analysis.deps_of_inputs(&*after, &[caller_place]);
          log::debug!("pin output {path:?} -> {caller_place:?}: {deps:?} at {at:?}");
          // Unchanged pointees do not explain a value written by this call.
          if !deps.contains(at) {
            continue;
          }
          let aliases = results
            .analysis
            .place_info
            .aliases(caller_place)
            .iter()
            .map(|alias| (*alias, LocationOrArg::Location(at)))
            .collect();
          let source = TargetDeps::all(&[aliases], results).pop().unwrap();
          if source.reaches(&backward) {
            next.extend(
              boundary
                .returns
                .iter()
                .map(|at| (*place, LocationOrArg::Location(*at))),
            );
          }
        }
      }
      if !next.is_empty() || controls_call {
        pending.push_back((callee, forward, next, controls_call));
      }
    }
  }
  let mut output = output.into_values().collect::<Vec<_>>();
  for item in &mut output {
    item.pre = merge_spans(std::mem::take(&mut item.pre));
    item.post = merge_spans(std::mem::take(&mut item.post));
  }
  output.sort_by_key(|item| tcx.hir_span(tcx.hir_body_owner(item.body)).lo());
  Ok(output)
}
