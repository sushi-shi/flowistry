# Continuation implementation progress

The [full plan](continuation-plan.md) remains the objective. No milestone is
complete merely because tooling or a subset of its tests passes.

The live highlighting report was addressed after #51 in
[#52](https://github.com/sushi-shi/flowistry/pull/52) and
[editor #5](https://github.com/sushi-shi/flowistry.nvim/pull/5): independent
constructor fields, comment colors, parameter-type selection and rustfmt-save
redraw. [Source-selection evidence](source-selection.md) includes the actual
gameplay constructor. These fixes do not complete the remaining milestones.
The coordinator work in `/tmp/flowistry-project-coordinator` now contains #53
through an ordinary integration merge. [Draft #54](https://github.com/sushi-shi/flowistry/pull/54)
targets that monorepo tip; see [the coordinator notes](project-coordinator.md).
The editor now lives under `nvim/` in this repository; backend and editor changes
share one package source, lock file, CI suite and PR chain. See
[neovim-monorepo.md](neovim-monorepo.md).

| Step | State | Evidence / next gate |
|---|---|---|
| 1. Combined validation | In progress, [#41](https://github.com/sushi-shi/flowistry/pull/41) | Full #17/integrated coverage: 2,827 equal successful pairs, 48 paired benign selections and five reference-OOM/integrated-success outcomes; see [combined-corpus-results.md](combined-corpus-results.md). Independent reference coverage is complete but five checks remain resource-limited. The legacy file-cache run completed with 2,878 equal successes and two decoder failures; the filename-fixed coordinator candidate now passes all 2,880 cache-off/refresh pairs. Warm-cache and final-tip gates remain. |
| 2. Performance baseline | In progress, [#42](https://github.com/sushi-shi/flowistry/pull/42) | Measurement tooling and 42 total harness regressions pass; real interleaved perf and isolated-replay probes work. Actual immediate-base, project and save measurements and budgets remain pending; see [measurement-progress.md](measurement-progress.md). |
| 3. Recurse row groups | Full Recurse equality passed, [#43](https://github.com/sushi-shi/flowistry/pull/43); acceptance pending | 1,416 equal successful pairs and 24 matching benign selections cover all locked Recurse positions. 107 core tests pass with engine-diff/shadow-eager. Full reference and quiet stress performance gates remain; see [recurse-row-groups.md](recurse-row-groups.md). |
| 4. Seed rows | Full corpus equality passed, [#44](https://github.com/sushi-shi/flowistry/pull/44); acceptance pending | All 2,880 locked selections agree: 2,832 successful pairs and 48 matching benign selections. 108 core tests pass; enhanced reference and quiet seed-construction/stress measurements remain. See [seed-row-experiment.md](seed-row-experiment.md). |
| 5. HIR hashing | Investigated, [#45](https://github.com/sushi-shi/flowistry/pull/45); no compiler change retained | Three normal-release profiles and pinned compiler source identify metadata-driven HIR owner hashing. Examined shortcuts do not bypass it safely; see [hir-hashing-experiment.md](hir-hashing-experiment.md). No speedup claimed. |
| 6. Edit/concurrency harness | Implemented in [#47](https://github.com/sushi-shi/flowistry/pull/47); release gates pending | 84 serial, concurrency, recovery and real-project cases pass; 47 harness regressions pass. See [edit-concurrency-validation.md](edit-concurrency-validation.md). Add versioned-publication/background cases as those features land; full corpus gates remain required. |
| 7. Shared result index | Implemented in [#48](https://github.com/sushi-shi/flowistry/pull/48); final integration gates pending | Existing caches now share a publication lock and disk budget. Compiler-derived body index, revision/generation envelopes, cancellation, source-hash checks and interrupted-write recovery pass 16 new cases, plus all 84 edit cases. See [versioned-result-index.md](versioned-result-index.md). |
| 8. Persisted summaries | Implemented in [#49](https://github.com/sushi-shi/flowistry/pull/49); full corpus gates pending | Portable schema, shared disk adapter/fingerprints and dependency/reverse snapshots pass 18 summary cases, all 84 edit cases, all 16 publication cases, workspace tests and 52 harness regressions at `9da7aba9f`. Full Recurse gate passed: 1,440 equal outputs and 18,008 structurally verified summary hits. Quiet measurements and final-tip gates remain. See [persisted-summary-progress.md](persisted-summary-progress.md). |
| 9. Project command | Implemented in [#51](https://github.com/sushi-shi/flowistry/pull/51) and [#54](https://github.com/sushi-shi/flowistry/pull/54); acceptance pending | Explicit Cargo targets, portable inventories and body workers feed a streaming coordinator with priorities, cancellation and bounded workers. All 16 coordinator and 14 worker scenarios also pass at the #56 candidate. Large-project and performance gates remain. See [coordinator evidence](project-coordinator.md). |
| 10. Neovim background work | Implemented in [#55](https://github.com/sushi-shi/flowistry/pull/55); acceptance pending | Candidate `24ed53ecb` passes all five package checks, 323 frontend assertions, 14 backend worker scenarios and real-worker editor tests in both modes, including actual packaged cancellation. Global worker and editor-retention bounds are enforced; large-project/performance and final gates remain. See [background notes](neovim-background.md). |
| 11. Incremental saves | Implemented increments [#56](https://github.com/sushi-shi/flowistry/pull/56) and [#57](https://github.com/sushi-shi/flowistry/pull/57); acceptance pending | #56 passes eight save scenarios and all 84 existing edit cases with selective solver work. #57 adds compiler-input editor invalidation: 366 frontend assertions, 31 live-editor assertions, 20 IDE tests and 16 publication cases pass. The [compiler-state experiment](rustc-incremental-experiment.md) passes 510 fresh-oracle comparisons but all measured variants use more instructions; none is retained. Quiet save measurements and expanded final edit/concurrency gates remain. See [save scheduling](dependency-save-plan.md) and [input invalidation](editor-input-invalidation.md). |
| 12. Layout reuse | Pending | Proven-safe cases only, compiler fallback otherwise. |
| 13. Final release gates | Pending | Full corpus, edits, packaged editor and before/after evidence. |

## Active validation and retained evidence

Worktree: `/tmp/flowistry-continuation`, branch `test/combined-chain-validation`.
Use the cached dev wrapper described in [handoff.md](handoff.md).

All new heavy artifacts use
`/home/sheep/Projects/flowistry/target/continuation-validation`:

- `baseline/`: backend `2630ee356`, normal release build.
- `reference17/`: backend `82d430807`, feature-complete reference.
- `reference-checks/`: backend `2630ee356` with
  `flowistry/engine-diff,flowistry/shadow-eager`.
- Each archive has `build.json` with source revision, compiler, flags and binary
  hashes. Only the two executables are copied; builds reuse `target/review-cache`.
- `equivalence.log`, `equivalence.json`, `equivalence-checkpoints/`: full locked
  corpus, both modes, #17 versus integrated backend, cache off, 6 GiB cap, two
  concurrent entries. The final JSON appears only at completion; per-position
  checkpoints persist meanwhile.
- `equivalence-harness.py` and `equivalence-checkpoint.py`: exact source snapshots
  of the initially launched run. Its manifest matches those snapshots. The
  subsequent #41 change stabilizes temporary paths for future/resumed launches;
  a changed environment deliberately gets a different checkpoint identity.
- `inherited-queue-transition.json`: old queue controllers were terminated by
  verified exact PID. The already-running timing helper (PID 133458, harness
  496111 at inspection) was allowed to finish; all inherited reports remain.
  Its load log marks overlap with the new builds, so it is not quiet timing
  evidence. The helper has now finished: 704 successful pairs, zero differences,
  and 16 paired benign results. Its freed slot runs the independent reference gate.
- `reference.log`, `reference.json`, `reference-checkpoints/`: independent engine
  and eager-reference corpus run, using the existing engine corpus, two workers
  and the 6 GiB cap. Its harness source is #41 commit `11d5f3763`.
- The first combined run predates internal-directory-symlink support. Alacritty
  and Bevy source aliases can fail preparation in that version. Keep completed
  checkpoints with their original manifest; missing entries require a fixed-harness
  rerun. No full coverage claim can be made from this partial evidence.
- Four empty-output reference failures are confirmed memory-cgroup OOM kills;
  one other just Recurse case timed out at 600 seconds. See
  [reference-failure-triage.md](reference-failure-triage.md). Compiler replay and
  snapshot wrappers now preserve signal termination for future runs. The frozen
  reference executable and raw recorded failures remain unchanged.

The combined run uses the already-prepared corpus at
`$S/h19/target/smoke-crates`; the inherited timing uses a separate engine corpus.
Do not start another run that writes to the same prepared corpus concurrently.
Do not delete active source/build inputs or preserved experiment work.

## Next concrete actions

1. The original combined run has ended. Its symlink exception canceled later queued
   entries and prevented final report assembly. `equivalence-recovered.json`
   retains 1,560 checksum-verified records under the original manifest: 1,513
   successful matching pairs, 42 paired benign outcomes and five #17 OOM →
   integrated-success cases. The recovery script also verified locked positions,
   saved harness hashes and current source hashes; see `recover-equivalence.py`.
   The corrected Alacritty/Bevy run has completed: 238 equal successful pairs and
   two paired benign selections in `equivalence-symlinks.{log,json}`. The other
   nine entries have also completed: 1,076 equal successful pairs and four paired
   benign selections in `equivalence-remaining.{log,json}`. The assembled
   `equivalence-complete{,-summary}.json` has exact full coverage; see
   [combined-corpus-results.md](combined-corpus-results.md). The full Recurse
   comparison of `baseline/` with `row-groups/` has passed: 1,416 equal successful
   pairs and 24 matching benign selections. The full both-mode `row-groups/`
   versus `seed-rows/` comparison also passed: 2,832 equal successful pairs and
   48 matching benign selections in `seed-rows-corpus.{log,json}` and
   `seed-rows-checkpoints/`. Its h19 slot now runs persisted-summary verification
   at `file-identities-v2/`, as described below, under the same two-job/6 GiB caps.
   Cache-off versus warm `file-focus` at `result-index-v1/` completed all 2,880
   requests successfully, but reported 2,040 output differences. A reproduced
   pair differs only in the requested file's response-local FilenameIndex.
   The comparator now resolves that anchored ID and rejects unresolved foreign
   numeric IDs. All 51 harness regressions pass. The original digests remain in
   `file-cache-corpus.{log,json}` and the compact
   [initial report](measurements/file-cache-initial.json); they are not relabeled
   as passing. The corrected full run uses `file-cache-resolved.{log,json}` and
   `file-cache-resolved-checkpoints/`. It encountered two legacy decoder failures
   at just's `src/unindent.rs:49:23`; both compiler commands exited successfully.
   The [#50 filename-table fix](file-focus-identities.md) resolves the foreign macro
   identity and passes 124 process-level cases at `76d775628`. Preserve the
   legacy run and its raw outcomes; the final cache corpus must use the new binary.
   See [file-cache-comparison.md](file-cache-comparison.md).
   Preserve all three manifests when assembling coverage; do not present a new
   harness manifest as the origin of old records.
   The independent reference sweep has finished with complete coverage: 2,827
   successes, 48 benign selections, four kernel-confirmed OOMs masked as crashes
   and one timeout. [Its report](measurements/reference-checks.json) retains all
   outcomes; five positions still lack successful reference validation.
2. Cross-launch reuse is proven by `resume-proof-{first,second}.json`: identical
   checkpoint identity, reused second record, preserved original timestamp.
   `cache-proof-stable.json` proves equal file-focus output with one snapshot hit
   versus a cache-off miss. These small probes do not establish full coverage.
3. When a heavy slot is free, measure the frozen row-group and seed candidates
   against their predecessors on the just stress cases, then complete their full
   corpus/reference gates. Normal binaries are frozen at `row-groups/` (`3199b4095`)
   and `seed-rows/` (`f1163469c`). Use isolated replay stores and warmup controls from
   #42. Small probes cannot decide whether to retain either optimization.
4. Resolve the combined reference's resource-limited cases with targeted bounded
   checks, then complete cache-off/fresh/warm and file-focus corpus equivalence.
   Keep raw failures and the kernel-backed classification, and distinguish partial
   evidence from a passed gate. Do not blindly repeat the same expensive failing run.
5. The existing-backend edit/concurrency matrix passes 84 cases, including ten
   curated real-project edits. Its instrumented normal executable is frozen at
   `edit-matrix-v1/` (`79930142f`); detailed results are `edit-matrix-complete.json`.
   Feature 7's candidate is frozen at `result-index-v1/` (`235760027`). It passes
   16 publication cases, the same 84 edit cases, and legacy cache regressions.
   Reports are `result-index-final.json` and `index-edit-matrix-final.json`.
   Continue with persisted summaries, project command, Neovim background work,
   saves and safe relocation (steps 8–12). Step 8's portable core boundary is
   connected to its disk adapter; 18 summary, 84 edit and 16 publication cases
   pass at `summary-store-v2/` (`9da7aba9f`). The full Recurse persisted/fresh
   logical-summary and output gate now runs at the filename-fixed
   `file-identities-v2/` (`76d775628`), using the h19 corpus and separate
   `summary-corpus-stores/`, `summary-corpus-checkpoints/` and
   `summary-corpus.{json,log}` artifacts.
   Use `FLOWISTRY_VERIFY_SUMMARIES=1` and `RUST_LOG=flowistry::audit=info` so
   completed-result hits cannot bypass summary verification and the report
   records actual summary hits/computations/verifications. Preserve both active
   corpus runs and their separate prepared roots; do not start a third heavy run.
   Step 9's [worker foundation](project-analysis-progress.md) is implemented and
   frozen at `project-workers-v2/` (`4b4915382`), with 137 passing process cases.
   Its public coordinator, streaming, priorities, cancellation and resource limits
   remain unimplemented, as do steps 10–12.
   Finish all final gates in step 13. Current continuation PR order is #41 → #42 →
   #43 → #44 → #45 → #46 → #47 → #48 → #49 → #50 → #51; nothing has been merged.

## Completed corpus runs and latest-stack continuation (2026-09-30)

Both previously active corpus jobs are terminal; their prepared roots are no
longer reserved by those jobs. Exact locked coverage, source/build manifests and
raw-report hashes are preserved in
[the legacy cache report](measurements/file-cache-resolved-complete.json) and
[the persisted-summary report](measurements/summary-corpus-complete.json).
The former has 2,878 matching successes and two zero-exit legacy decoder failures
at just unindent.rs:49:23 (both modes), retained under their raw crash labels.
The latter passes all 1,440 Recurse requests and all 18,008 summary-hit structural
checks. Neither run supplies quiet performance evidence or final-tip validation.

Current continuation code starts from #53 in `feat/project-coordinator`, preserving
the earlier WIP commit and merging `refactor/neovim-monorepo` normally. #53's paired
package CI passed. No PR was merged and no history was force-pushed. The earlier
'active run' descriptions above are historical; these terminal reports supersede
them. Step 9's large-project/performance gates and steps 10–13 remain unfinished.

The next candidate is frozen as `project-coordinator-v1/` at `2fae9dd12`.
All 16 public coordinator and 13 worker scenarios pass, including kernel-backed
OOM handling, setsid descendant cancellation, superseded saves and blocked output.
The IDE suite passes 38 tests and the Python suite 55. See
[the exact candidate evidence](measurements/project-coordinator.json).
Two new heavy runs use this immutable candidate and the metadata-aware comparator:
`coordinator-cache-refresh.{log,json}` with the engine prepared corpus, and
`coordinator-cache-warm.{log,json}` with h19. Each covers all locked selections in
both modes with cache-off as its oracle and has its own `-stores/` and
`-checkpoints/` namespace. They use two jobs and a 6 GiB per-request cap; do not
start a third heavy harness or write to either prepared corpus while they run.
Summary verification mode is disabled for these ordinary cache gates. Retained
harness snapshots are `coordinator-cache-harness.py` and
`coordinator-cache-checkpoint.py` under the validation root.

## Background editor continuation above #54

`/tmp/flowistry-background`, branch `feat/neovim-background`, is now draft #55
based directly on `feat/project-coordinator`. The runtime candidate is frozen in
`neovim-background-v2/` at `24ed53ecb`. The standalone source clone
`/tmp/flowistry-background-package` validated all five root Nix checks and the
actual launcher at that same revision; roots are
`/tmp/flowistry-background-validation-v2{,-1,-2,-3,-4,-5}`. Later commits add the
packaged cancellation fixture, evidence and review links, with runtime files
verified byte-identical to the candidate. Exact hashes and outcomes are in
[the background report](measurements/neovim-background.json).

#54's GitHub paired-package CI passed. #55 remains draft while its acceptance gates
continue. The editor's background mode is opt-in; its one global background slot
is retained through process cleanup and pending decompression. Frontend retention
is bounded by decoded JSON bytes, not presented as a hard Lua RSS guarantee.
The current saves conservatively restart project inventories. Continue step 11
with dependency-aware current-function → saved-file → affected-caller scheduling,
then step 12's proven-safe layout reuse and all remaining measurement/release gates.
The two coordinator cache corpus jobs remain live in their existing prepared roots;
do not replace their frozen executables, mutate those roots, or start a third heavy
validation harness until a slot is authoritatively free.

## Latest stack and cache-refresh milestone

The newest published PR is [#58](https://github.com/sushi-shi/flowistry/pull/58),
branch `experiment/rustc-incremental` in `/tmp/flowistry-rustc-incremental`.
It targets #57 (`fix/editor-input-invalidation`, `0d0108a96`) directly; #57's
final paired-package CI passed. #58 preserves the experiment and reports, with
production Rust and editor sources identical to #57. New work starts above #58.

The coordinator cache-refresh run has completed with exact locked coverage:
24 crates × 60 positions × two modes, 2,880 equal successful protocol responses,
no output differences or status changes. There were 2,832 analyzed cache misses
per side and 48 successful selections without an analyzed body. See
[the retained report](measurements/coordinator-cache-refresh-complete.json).
This tests frozen `2fae9dd12`, not the later final stack; loaded timings are not
performance acceptance evidence. The engine prepared root is no longer reserved
by this completed run. The separate h19 warm-cache job remains live; preserve
its source/build inputs and checkpoints. Earlier descriptions of both jobs as
live are historical.


The compiler-state experiment is complete for the tested variants: 510 fresh-oracle
comparisons pass and all 320 measured samples have instruction counters.
Finalized reuse still adds 1.4–5.7% instructions on the two real-project semantic
cache cases. No runtime compiler change is retained; see
[the experiment](rustc-incremental-experiment.md) and its exact raw provenance.
Continue with step 12 and the outstanding gates in steps 1–4, 6–11 and 13.
