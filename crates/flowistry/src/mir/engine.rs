//! This module re-implements [`rustc_mir_dataflow::Engine`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/struct.Engine.html) for performance reasons.
//!
//! The information flow analysis has a large domain of size `O(|places| * |locations|)`.
//! It has two engines:
//!
//! * [`iterate_to_fixpoint`] stores the state at the entry of every basic block only: a
//!   visit of a block runs its statements and terminator on one working state, which
//!   it then joins into the entries of the block's successors. Blocks are visited in
//!   reverse postorder priority (the dirty block that comes first in reverse postorder
//!   is visited next), so loops are iterated before the code after them. The state at
//!   a [`Location`] is recomputed on demand from the entry of its block (see
//!   [`AnalysisResults::state_at`]); a few recently expanded blocks are kept, and
//!   [`AnalysisResults::for_each_state`] replays all blocks once, in order.
//!
//! * [`iterate_to_fixpoint_by_location`] materializes the state of every location, and
//!   applies the effect of a location to its own state joined with its predecessors'
//!   (so the effect's previous output is part of its next input). Profiling showed that
//!   allocating, cloning and dropping states dominated rustc's engine, which this
//!   avoids at the cost of memory.
//!
//! Both compute the same states if applying the effect of every location to its own
//! output adds nothing, i.e. `f(x ∨ f(x)) = f(x)` for the effect `f` of every location
//! (see `FlowAnalysis::unstable_locations`). Otherwise, the result of the location
//! engine depends on how often it revisits a location, which the block engine cannot
//! reproduce.

use std::{
  cell::RefCell,
  cmp::Reverse,
  collections::{BinaryHeap, VecDeque},
  ops::Deref,
  rc::Rc,
};

use either::Either;
use indexical::ToIndex;
use rustc_data_structures::{graph::Successors, work_queue::WorkQueue};
use rustc_index::{IndexVec, bit_set::DenseBitSet};
use rustc_middle::{
  mir::{BasicBlock, Body, Location, traversal},
  ty::TyCtxt,
};
use rustc_mir_dataflow::{Analysis, Direction, JoinSemiLattice};
use rustc_utils::{
  BodyExt,
  mir::location_or_arg::{
    LocationOrArg,
    index::{LocationOrArgDomain, LocationOrArgIndex},
  },
};

/// The number of expanded blocks (states of every location of a block) that
/// [`AnalysisResults::state_at`] keeps.
const EXPANDED_BLOCKS: usize = 16;

/// An alternative implementation of
/// [`rustc_mir_dataflow::Results`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/struct.Results.html).
pub struct AnalysisResults<'mir, 'tcx, A: Analysis<'tcx>> {
  /// The underlying analysis that was used to generate the results.
  pub analysis: A,
  body: &'mir Body<'tcx>,
  location_domain: Rc<LocationOrArgDomain>,
  storage: Storage<A::Domain>,
  engine_stats: EngineStats,
}

enum Storage<D> {
  /// The state at the entry of every block that the analysis reached (`None` for
  /// unreachable blocks, whose states are all the bottom value); see
  /// [`iterate_to_fixpoint`].
  Blocks {
    entries: IndexVec<BasicBlock, Option<D>>,
    bottom: D,
    /// Recently expanded blocks, most recent first.
    expanded: RefCell<VecDeque<(BasicBlock, Rc<[D]>)>>,
  },
  /// The states after every location of every block; see
  /// [`iterate_to_fixpoint_by_location`].
  Locations(IndexVec<BasicBlock, Rc<[D]>>),
}

/// Counters of one run of a fixpoint engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
  /// Whether the block engine computed the results (else the location engine).
  pub by_block: bool,
  /// How many times the analysis applied the effect of a statement or terminator,
  /// counting revisits.
  pub location_visits: usize,
  /// How many times the block engine visited a block.
  pub block_visits: usize,
  /// How many joins into a successor's state changed it.
  pub changed_joins: usize,
}

/// The state of an [`Analysis`] after a location, see [`AnalysisResults::state_at`].
///
/// It dereferences to the state. It keeps the states of its block alive, not the
/// results.
pub struct StateRef<D> {
  states: Rc<[D]>,
  index: usize,
}

impl<D> Deref for StateRef<D> {
  type Target = D;

  fn deref(&self) -> &D {
    &self.states[self.index]
  }
}

impl<'mir, 'tcx, A: Analysis<'tcx>> AnalysisResults<'mir, 'tcx, A> {
  /// Gets the computed [`AnalysisDomain`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/trait.AnalysisDomain.html)
  /// after a given [`Location`].
  ///
  /// With the block engine, the states of the location's block are recomputed from
  /// the block's entry unless the block was expanded recently; to visit the states of
  /// every location, prefer [`for_each_state`](Self::for_each_state).
  pub fn state_at(&self, location: Location) -> StateRef<A::Domain> {
    StateRef {
      states: self.block_states(location.block),
      index: location.statement_index,
    }
  }

  /// Calls `f` with every location of the body and the state after it, block by block
  /// in the order of [`Body::basic_blocks`], and in order within a block.
  pub fn for_each_state(&self, mut f: impl FnMut(Location, &A::Domain)) {
    for (block, data) in self.body.basic_blocks.iter_enumerated() {
      let locations = (0 ..= data.statements.len()).map(|statement_index| Location {
        block,
        statement_index,
      });
      match &self.storage {
        Storage::Locations(states) => {
          for (location, state) in locations.zip(states[block].iter()) {
            f(location, state);
          }
        }
        Storage::Blocks {
          entries, bottom, ..
        } => match &entries[block] {
          None => {
            for location in locations {
              f(location, bottom);
            }
          }
          Some(entry) => {
            let mut state = entry.clone();
            replay(&self.analysis, self.body, block, &mut state, &mut f);
          }
        },
      }
    }
  }

  /// Counters of the fixpoint iteration that computed these results.
  pub fn engine_stats(&self) -> EngineStats {
    self.engine_stats
  }

  /// The [`LocationOrArgDomain`] of the body.
  pub fn location_domain(&self) -> &Rc<LocationOrArgDomain> {
    &self.location_domain
  }

  /// The states after every location of `block`.
  fn block_states(&self, block: BasicBlock) -> Rc<[A::Domain]> {
    let (entries, bottom, expanded) = match &self.storage {
      Storage::Locations(states) => return Rc::clone(&states[block]),
      Storage::Blocks {
        entries,
        bottom,
        expanded,
      } => (entries, bottom, expanded),
    };
    let mut expanded = expanded.borrow_mut();
    if let Some(i) = expanded.iter().position(|(b, _)| *b == block) {
      let entry = expanded.remove(i).unwrap();
      let states = Rc::clone(&entry.1);
      expanded.push_front(entry);
      return states;
    }
    let len = self.body.basic_blocks[block].statements.len() + 1;
    let states: Rc<[A::Domain]> = match &entries[block] {
      None => vec![bottom.clone(); len].into(),
      Some(entry) => {
        let mut states = Vec::with_capacity(len);
        let mut state = entry.clone();
        replay(&self.analysis, self.body, block, &mut state, |_, state| {
          states.push(state.clone())
        });
        states.into()
      }
    };
    expanded.push_front((block, Rc::clone(&states)));
    expanded.truncate(EXPANDED_BLOCKS);
    states
  }
}

/// Applies the effects of the statements and the terminator of `block` to `state`,
/// calling `f` after each location.
fn replay<'tcx, A: Analysis<'tcx>>(
  analysis: &A,
  body: &Body<'tcx>,
  block: BasicBlock,
  state: &mut A::Domain,
  mut f: impl FnMut(Location, &A::Domain),
) {
  let data = &body.basic_blocks[block];
  for (statement_index, statement) in data.statements.iter().enumerate() {
    let location = Location {
      block,
      statement_index,
    };
    analysis.apply_primary_statement_effect(state, statement, location);
    f(location, state);
  }
  let location = Location {
    block,
    statement_index: data.statements.len(),
  };
  analysis.apply_primary_terminator_effect(state, data.terminator(), location);
  f(location, state);
}

/// Runs a given [`Analysis`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/trait.Analysis.html)
/// to a fixpoint over the given [`Body`], storing the state at the entry of every block
/// (see the [module documentation](self)).
///
/// A reimplementation of [`rustc_mir_dataflow::framework::engine::iterate_to_fixpoint`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_mir_dataflow/framework/engine/struct.Engine.html#method.iterate_to_fixpoint)
/// for forward analyses.
pub fn iterate_to_fixpoint<'mir, 'tcx, A: Analysis<'tcx>>(
  _tcx: TyCtxt<'tcx>,
  body: &'mir Body<'tcx>,
  location_domain: Rc<LocationOrArgDomain>,
  analysis: A,
) -> AnalysisResults<'mir, 'tcx, A> {
  assert!(
    A::Direction::IS_FORWARD,
    "the engine only runs forward analyses"
  );
  let bottom = analysis.bottom_value(body);

  // The position of every reachable block in reverse postorder: blocks are visited in
  // that order of priority.
  let blocks = &body.basic_blocks;
  let rpo = traversal::reverse_postorder(body)
    .map(|(block, _)| block)
    .collect::<Vec<_>>();
  let mut priority = IndexVec::from_elem_n(usize::MAX, blocks.len());
  for (i, block) in rpo.iter().enumerate() {
    priority[*block] = i;
  }

  let mut entries: IndexVec<BasicBlock, Option<A::Domain>> =
    IndexVec::from_elem_n(None, blocks.len());
  for block in &rpo {
    entries[*block] = Some(bottom.clone());
  }
  analysis.initialize_start_block(body, entries[rpo[0]].as_mut().unwrap());

  let mut dirty = DenseBitSet::new_empty(blocks.len());
  let mut queue = BinaryHeap::new();
  for block in &rpo {
    dirty.insert(*block);
    queue.push(Reverse(priority[*block]));
  }

  let mut engine_stats = EngineStats {
    by_block: true,
    ..EngineStats::default()
  };
  while let Some(Reverse(i)) = queue.pop() {
    let block = rpo[i];
    if !dirty.remove(block) {
      continue;
    }
    engine_stats.block_visits += 1;
    engine_stats.location_visits += blocks[block].statements.len() + 1;

    let mut state = entries[block].as_ref().unwrap().clone();
    replay(&analysis, body, block, &mut state, |_, _| {});

    for successor in blocks.successors(block) {
      let entry = entries[successor].as_mut().unwrap();
      if entry.join(&state) {
        engine_stats.changed_joins += 1;
        if dirty.insert(successor) {
          queue.push(Reverse(priority[successor]));
        }
      }
    }
  }

  AnalysisResults {
    analysis,
    body,
    location_domain,
    storage: Storage::Blocks {
      entries,
      bottom,
      expanded: RefCell::new(VecDeque::with_capacity(EXPANDED_BLOCKS)),
    },
    engine_stats,
  }
}

/// Runs a given [`Analysis`] to a fixpoint over the given [`Body`], materializing the
/// state of every location (see the [module documentation](self)).
pub fn iterate_to_fixpoint_by_location<'mir, 'tcx, A: Analysis<'tcx>>(
  _tcx: TyCtxt<'tcx>,
  body: &'mir Body<'tcx>,
  location_domain: Rc<LocationOrArgDomain>,
  analysis: A,
) -> AnalysisResults<'mir, 'tcx, A> {
  assert!(
    A::Direction::IS_FORWARD,
    "the engine only runs forward analyses"
  );
  let bottom_value = analysis.bottom_value(body);

  // `state` materializes the analysis domain for *every* location, which is the crux
  // of this implementation strategy.
  let num_locs = body.all_locations().count();
  let mut state = IndexVec::from_elem_n(bottom_value, num_locs);

  analysis
    .initialize_start_block(body, &mut state[Location::START.to_index(&location_domain)]);

  let mut dirty_queue: WorkQueue<LocationOrArgIndex> = WorkQueue::with_none(num_locs);
  for (block, data) in traversal::reverse_postorder(body) {
    for statement_index in 0 ..= data.statements.len() {
      let location = Location {
        block,
        statement_index,
      };
      dirty_queue.insert(location.to_index(&location_domain));
    }
  }

  let mut engine_stats = EngineStats::default();
  while let Some(loc_index) = dirty_queue.pop() {
    let LocationOrArg::Location(location) = *location_domain.value(loc_index) else {
      unreachable!()
    };
    engine_stats.location_visits += 1;
    let next_locs = match body.stmt_at(location) {
      Either::Left(statement) => {
        analysis.apply_primary_statement_effect(
          &mut state[loc_index],
          statement,
          location,
        );
        vec![location.successor_within_block()]
      }
      Either::Right(terminator) => {
        analysis.apply_primary_terminator_effect(
          &mut state[loc_index],
          terminator,
          location,
        );
        body
          .basic_blocks
          .successors(location.block)
          .map(|block| Location {
            block,
            statement_index: 0,
          })
          .collect::<Vec<_>>()
      }
    };

    for next_loc in next_locs {
      let next_loc_index = location_domain.index(&LocationOrArg::Location(next_loc));
      let (cur_state, next_state) = state.pick2_mut(loc_index, next_loc_index);
      if next_state.join(cur_state) {
        engine_stats.changed_joins += 1;
        dirty_queue.insert(next_loc_index);
      }
    }
  }

  // Group the states by block. Locations are indexed block by block, in order.
  let mut states = state.into_iter();
  let states = body
    .basic_blocks
    .iter()
    .map(|data| {
      states
        .by_ref()
        .take(data.statements.len() + 1)
        .collect::<Rc<[_]>>()
    })
    .collect();

  AnalysisResults {
    analysis,
    body,
    location_domain,
    storage: Storage::Locations(states),
    engine_stats,
  }
}
