# Feature-by-feature continuation plan

This is the execution plan for all three areas requested after assembling the
[review chain](review-chain.md): combined-chain validation and measurements,
remaining solver optimizations, and background/project analysis with incremental
save handling. It supersedes the phase ordering in the earlier
[incremental design](incremental-analysis-plan.md).

The persistent semantic cache and snapshot replay are already implemented in
backend #2. The starting backend is `2630ee356`; the matching Neovim tip is
`34c447e`. Documentation continues in #40. Existing PRs remain unmerged until
review. New backend PRs form a single continuation after #40; editor PRs continue
after nvim#4, with their backend dependencies and compatible pins recorded.

Each numbered item is a reviewable feature or evidence milestone. Split a feature
into consecutive PRs if necessary; preserve the sequence and its acceptance gate.
An optimization experiment is complete when evidence establishes either a safe
improvement worth retaining or a documented reason to reject it. Shipping slower
or semantically different code is not required to call the experiment complete.

## 1. Reproducible combined-chain validation

**Deliverable:** a pinned validation manifest, resumable corpus reports, and a
resolved list of output/status differences for the integrated backend.

- Inventory the inherited pipelines and reuse their completed reports. Clearly
  distinguish original-branch evidence from evidence about the combined chain.
- Use the corrected #19 canonicalizer on all 25 locked corpus entries, both
  SigOnly and Recurse, and all locked positions. Record missing prerequisites,
  skips, failures, timeouts and OOMs; successful-pair equality alone is not enough.
- Compare the combined chain with the feature-complete #17 reference where the
  semantics should agree. Master is a historical performance reference, not a
  zero-difference oracle for the intentional #14–#17 precision changes.
- Classify demand-driven error-handling changes separately. Explain every output
  change; never bless all differences or silently drop failed positions.
- Run the independent engine reference and lazy/eager checks across the corpus.
  Compare cache-off analysis against fresh-cache and warm-cache results at the
  final tip, including file-focus and its indexed maybe-slices.
- Use a representative set for rapid iteration, but complete the locked corpus
  gate before declaring this milestone done. Retest the affected PR/base pair
  when a difference needs localization.

**Done when:** no unexplained semantic differences, no unclassified status
changes, no unexpected compiler crashes, and every corpus entry has a recorded
outcome. Retain a durable compact report with exact revisions, toolchain, flags,
corpus revisions, commands and references to detailed results.

## 2. Combined-chain performance baseline and resource budgets

**Depends on:** 1 for correctness; measurement preparation can happen alongside it.

**Deliverable:** cold-analysis, warm-reuse, whole-project and save-latency baselines,
plus measured stress budgets replacing the placeholders.

- Separate cold build, warm compiler with cache off, compiler-validated cache hit,
  and snapshot replay. A cache hit must not appear as a solver improvement.
- Measure Cargo/startup, frontend, borrow facts, summary construction, seed rows,
  solving, dependency queries, span/output construction and serialization.
- Measure typical requests and the just/niri stress cases, both modes, file-focus,
  and a whole-project baseline obtained by visiting source files.
- Measure save-to-visible-result latency for representative semantic and layout
  edits. Report repeated interleaved instruction counts, peak RSS, output size,
  wall-time median and tail latency, and disk growth. Use quiet runs for latency
  claims; keep loaded-machine observations labeled.
- Compare optimization PRs to their actual immediate predecessors. Reuse one
  build area and archive only executables needed for comparisons, rather than
  retaining a full debug tree per PR.

**Done when:** reports identify the dominant costs and reproducible gains or
regressions; budgets include measured headroom and explicit hardware context.
Use 300 ms as a provisional warm save-to-result target, not a claim already met.
Report stress cases separately. Small reproducible gains remain worthwhile.

## 3. Recurse row groups

**Depends on:** 1–2.

**Deliverable:** port the useful change from `perf/recurse-groups` onto the current
chain, without importing the experiment branch's unrelated harness/history.

Store groups of callee-written leaves sharing a value without materializing every
row. Preserve partial/strong updates, joins, projections, shared-handle effects,
recursive-call fallbacks and existing summary semantics. Keep logical effects
independent of the physical grouped representation so later persistence can use a
stable wire schema.

**Done when:** existing summary tests, engine/reference checks and the full Recurse
corpus agree with the preceding implementation; stress RSS and stored-row counts
improve reproducibly without material typical-request regressions. Recheck the
historical 20.3M-to-1.17M row result on the integrated chain. Keep the block-only
experiment separate: it intentionally changes a fixture and is not an exact
optimization to fold into this PR.

## 4. Faster seed-row construction

**Depends on:** 3, to measure against the final row representation.

**Deliverable:** avoid repeatedly enumerating overlapping argument-place subtrees
while constructing `SeedRows`.

Cache or share enumeration only with the full traversal context needed to preserve
the type-stack cutoff. A child-rooted traversal can legally go deeper than that
same child reached from a parent, so caching by place alone is insufficient.
Cover nested references, recursive types, enums, arrays and field strong updates.

**Done when:** `shadow-eager` and the modification/alias fixtures agree exactly,
the full corpus has no unexplained changes, and seed construction improves on
just's large argument type without trading it for excessive retained memory.

## 5. HIR hashing experiment

**Depends on:** 2; measured and stacked after 4.

**Deliverable:** a profile-backed reduction of avoidable HIR/metadata work, or a
committed experiment report explaining why no safe improvement was found.

Identify the query that triggers hashing in this pinned compiler and distinguish
compiler metadata hashing from the cache's own correctness fingerprints. Evaluate
query scheduling or compiler options only if they preserve target/crate semantics,
macro behavior, dependency metadata and cache validity. Do not remove validation
hashes to improve timings. Do not adopt the previously slower cdylib approach.

**Done when:** retained code reduces instruction counts across relevant library
and binary targets, with exact output and invalidation tests passing; otherwise
record the rejected variants and keep the current implementation.

## 6. Edit-and-concurrency validation harness

**Depends on:** 1; land after 5 in the review sequence.

**Deliverable:** a reusable modification matrix comparing every reuse path to an
independent fresh analysis, with cache-hit and compiler/solver-invocation counters.

Cover layout changes, comments and rustfmt; literals and control flow; callee
edits and recursive cycles; signatures/types/trait resolution; file moves and
module additions/removals; cfg/features, dependencies, build scripts, includes,
environment and proc macros; compile errors, recovery and undo. Exercise closures,
Unicode and maybe-slices. Run edits in isolated corpus copies.

Add races: two quick saves, a save during background analysis, overlapping editor
and background requests, cancellation, restart and interrupted cache writes.
Record which bodies were recomputed as well as output equality. New background
features add their scenarios to this harness as they land.

**Done when:** existing caches pass the applicable matrix and the harness can
reliably detect stale results, stale completion delivery and unnecessary solver
work. Run the full matrix on controlled fixtures plus curated real-project edits;
run corpus equality at each release gate.

## 7. Shared result index and versioned publication

**Depends on:** 6.

**Deliverable:** extend the existing result store for project and editor consumers;
keep current focus/file-focus response formats compatible.

Index validated results by package/target, configuration, mode and body identity.
Define source/input revisions and request generations. Publish each result
atomically with its validation provenance; readers accept only a matching current
revision. Canceled or superseded work cannot publish a result as current. Preserve
cache-off/refresh, corruption fallback, concurrent-writer safety and bounded disk
usage; interrupted runs can reuse completed, still-valid entries.

Do not persist process-local rustc identifiers. Body moves or ambiguous identities
must cause a safe miss until current compiler validation resolves them.

**Done when:** concurrent readers/writers, restarts, corruption and rapid-save
cases pass; stale results are rejected and cache growth respects its configured
limit. No second independent cache with different invalidation rules is introduced.

## 8. Persisted callee summaries and dependency graph

**Depends on:** 3, 6–7.

**Deliverable:** reuse unchanged callee analysis across compiler processes and
identify which callers need invalidation after an edit.

Serialize an explicitly portable summary schema, auditing every field rather than
assuming that a type without a lifetime is automatically safe to persist. Key it
by backend/compiler schema, mode/configuration, semantic inputs and dependency
fingerprints. Preserve recursive-component handling and fallback reasons. Rebuild
call resolution and dependency edges when the compiler's current inputs require
it. Maintain reverse edges for callers; conservative invalidation is the fallback
for declarations, trait resolution, macros or unknown dependency changes.

**Done when:** persisted and newly computed summaries agree; editing a callee
invalidates affected Recurse callers, cycles remain finite, and unaffected
summaries are reused after process restart. SigOnly is not needlessly coupled to
ordinary callee-body changes. Validate with feature 6 and the Recurse corpus.

## 9. Project-analysis command and bounded workers

**Depends on:** 7–8.

**Deliverable:** `cargo flowistry project` with explicit package/target selection,
priority files/bodies, a versioned stream of per-body outcomes, and cache filling.

Analyze the cursor body first, then open/saved files, then other bodies. Stream
successful results and diagnostics without waiting for the project to finish.
Keep existing focus/file-focus protocols intact; negotiate or explicitly select
the new stream. Support cancellation, resumption and partial failure.

Bound concurrency and memory. Release Flowistry-owned per-body state promptly,
but do not assume rustc arenas/query caches or borrow facts can be freed within a
live compiler session. Measure their growth; partition work into restartable
worker batches where needed. Avoid parallel Cargo invocations racing on a shared
target directory. One enormous body must fail within the cap without taking down
the editor or silently abandoning the remaining project.

**Done when:** each successful body matches standalone analysis; priorities and
partial output are observable; cancellation stops descendants; resume preserves
valid work; large projects stay inside the resource budget and report all body
outcomes. Compare total project time and time to first useful result with feature 2.

## 10. Background analysis in Neovim

**Depends on:** 9; editor PR after nvim#4, pinned to its tested backend.

**Deliverable:** one coordinated background job per enabled workspace, progress,
foreground priority, and analysis available across functions without repeated
per-function enabling.

Use the existing generation/change-tick guards and cancellation machinery. Open
buffers and the current cursor outrank background warming. Share validated store
results, coalesce duplicate requests, and prevent background activity from
starving interactive work. Integrate batch behavior with measured memory limits
instead of simply removing the current 600-line guard. Preserve a global disable
control and distinguish saved analysis from current-buffer state.

**Done when:** navigation, multiple buffers/workspaces, workspace changes, restart,
cancellation and compiler failures behave correctly in headless editor tests;
foreground latency and total background resource use are measured. Update the Nix
pin and test the packaged integration before enabling this behavior by default.

## 11. Dependency-aware incremental save handling

**Depends on:** 6–10.

**Deliverable:** save-triggered revalidation and selective recomputation, ordered
current function → saved file → affected callers elsewhere.

Coalesce rapid saves and cancel obsolete generations. Revalidate build/configuration
inputs, then fingerprint bodies and compare with the stored dependency graph.
Reuse validated unchanged results and persisted summaries; solve changed bodies
and invalidated Recurse callers. Declaration or unresolved-dependency changes may
require broader invalidation. Never promise that compiler frontend/borrowck work
is skipped merely because the Flowistry solver is skipped.

Measure whether rustc incremental caching actually helps with the demand-driven
callbacks and borrowck override; use supported reuse only when it is demonstrated.
Keep previous highlights explicitly stale while waiting, and suppress obsolete
completions. Publish a new result only for the input revision it analyzed.

**Done when:** the complete modification/concurrency matrix passes; unchanged
validated bodies do not rerun the solver unless their dependencies or context
changed; affected callers update; errors/revert/undo recover correctly. Publish
save-latency distributions and the reasons for every conservative fallback.

## 12. Safe layout-only reuse before compiler startup

**Depends on:** 6–8 and 11.

**Deliverable:** extend token-relative relocation to a proven-safe no-compile path
for eligible layout edits. Exact unchanged-input snapshot replay already exists.

First validate the complete relevant input snapshot and prove the edit preserves
all semantic inputs for the supported case. Token identity alone does not prove
this: line/column-sensitive macros, procedural macros, source includes, doc
comments and build scripts can observe source changes. Unsupported or uncertain
cases return to compiler validation. Do not silently broaden the set of ignored
comments or weaken snapshot checks.

**Done when:** eligible edits relocate exactly, including Unicode and maybe-slices,
and instrumentation proves zero compiler invocations. The same edits with
source-sensitive constructs either invalidate correctly or take the conservative
path. Document eligibility and fallback reasons; an unconditional “formatting
never compiles” promise is explicitly not an acceptance criterion.

## 13. Final validation, measurements and release readiness

**Depends on:** all retained changes from 1–12.

**Deliverable:** repeat the combined-chain corpus, reference-engine checks,
modification matrix and editor/package checks at exact final revisions. Publish
before/after cold, warm, project and save results, memory/disk budgets and known
limits. Confirm every PR diff is incremental and each editor pin is compatible.

**Done when:** all correctness gates pass, performance claims are backed by
repeatable measurements, remaining exceptions are explicitly recorded, and the
owner can review the full extended chain in order. No merge is implied.

## Execution and resource rules

- Start with milestones 1–2. Keep a frozen executable baseline before changing the
  solver, so optimizations cannot hide integration regressions.
- Work serially by default. Reuse shared corpus sources and build dependencies;
  retain reference/candidate release binaries and compact durable reports.
- At most two heavy harnesses at once; comparisons use `--memory-limit 6G -j 2`.
  Time-sensitive benchmarks get the machine to themselves. Check the inherited
  queues before starting more work.
- Check available disk space before builds. Do not recreate the deleted collection
  of per-PR debug directories. Remove only confirmed-unused, reproducible artifacts
  after preserving evidence and checking active users, following the owner's rule.
- For each feature: implement, run focused regressions, complete its stated gate,
  record measurements, and add one small PR or contiguous PR stack. Normal pushes
  are authorized; force pushes and merges retain the existing review rules.

Cross-function pin propagation, a long-running daemon, fixed-point warm starts and
adopting the semantically different block-only engine remain separate research
items. They are not prerequisites for completing these three requested areas.
