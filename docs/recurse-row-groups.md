# Recurse row-group port

This ports only experiment commit `ab7857370` onto the integrated review chain,
after the validation and measurement work. It is preparation for continuation
step 3; reference and measured acceptance gates remain open.

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
The full normal-release Recurse comparison against `2630ee356` has completed:
all 24 locked entries and 1,440 selections, with 1,416 equal successful pairs
and 24 matching benign selections. There are no output differences, missing
positions, duplicate records or skipped entries. The exact manifest and report
checksum are in [measurements/row-groups-corpus.json](measurements/row-groups-corpus.json).
Detailed evidence is `target/continuation-validation/row-groups-corpus.{json,log}`.
No extra corpus target directory was created.

Loaded-run aggregate wall time was 1,646.84 → 1,405.56 seconds, but the geometric
mean ratio was 0.994; peak RSS was 3,116 → 3,123 MiB. These observations do not
establish a reproducible performance gain or satisfy the retention gate.

Before accepting the optimization: finish baseline gates, run the enhanced reference build,
and measure typical and stress instructions/RSS/stored rows. The historical
20.3M-to-1.17M row result is not yet reproduced on this integrated branch. Reuse
the shared Cargo target directory and archive only the relevant executables.
