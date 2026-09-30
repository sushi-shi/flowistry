# Recurse row-group port

This ports only experiment commit `ab7857370` onto the integrated review chain,
after the validation and measurement work. It is preparation for continuation
step 3; the full corpus and measured acceptance gates remain open.

A callee returning a large aggregate can write thousands of destination leaves
with the same dependency set. A per-body group layout lets each dataflow state
store that set once. Reads resolve a member through its group; a separate write
to a member expands the group back into ordinary rows. Joins preserve the logical
values regardless of representation. The implementation retains exact/strong
update conditions, provenance checks and a row-by-row fallback; shared-handle
effects prevent grouping where they could change writes.

Integration changes beyond the original experiment:

- The current instability scan stops at its first failure when counters are off.
  Group preparation explicitly discovers reachable calls, preserving the bounded
  scan without depending on it to populate all call effects.
- Matrix equality resolves each operand's own group layout, so comparison with an
  ungrouped reference is symmetric and cannot index the wrong layout.
- For grouped bodies using the location engine, `engine-diff` first checks an
  independent ungrouped location execution, including terminator reads, before its
  existing cross-engine diagnostic. A legitimate block/location difference must
  not conceal a representation regression. Reference states are dropped between
  checks, and reference sessions do not contaminate production counters.

The original slicing fixtures cover whole destinations, individual member writes
and forward slicing. Randomized matrix operations compare groups against eager
rows. The aggregate test compares every state with ungrouped analysis, with three
call groups and fewer stored rows. An additional regression checks equality in
both operand orders.

All 107 core unit/integration tests pass with `engine-diff,shadow-eager` after the
integration fixes. The initial port also passed all-targets benchmark smoke tests.
The normal release executables at `3199b4095` are archived with build provenance
under `target/continuation-validation/row-groups/`, built in `target/review-cache`.
This frozen candidate can be compared after the active baseline gates release a
heavy-run slot; no extra corpus target directory was created.

Before accepting the optimization: finish baseline gates, compare to the frozen
integrated binary over the full Recurse corpus, run the enhanced reference build,
and measure typical and stress instructions/RSS/stored rows. The historical
20.3M-to-1.17M row result is not yet reproduced on this integrated branch. Reuse
the shared Cargo target directory and archive only the relevant executables.
