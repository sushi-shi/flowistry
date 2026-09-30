# Continuation implementation progress

The [full plan](continuation-plan.md) remains the objective. No milestone is
complete merely because tooling or a subset of its tests passes.

| Step | State | Evidence / next gate |
|---|---|---|
| 1. Combined validation | In progress, [#41](https://github.com/sushi-shi/flowistry/pull/41) | Frozen #17 and integrated builds; 26 validation regressions pass. Reference sweep has full coverage with five resource-limited outcomes; missing comparison entries are rerunning after old-harness recovery. Full cache-mode/file-focus gates pending. |
| 2. Performance baseline | In progress, [#42](https://github.com/sushi-shi/flowistry/pull/42) | Measurement tooling and 42 total harness regressions pass; real interleaved perf and isolated-replay probes work. Actual immediate-base, project and save measurements and budgets remain pending; see [measurement-progress.md](measurement-progress.md). |
| 3. Recurse row groups | Port prepared, [#43](https://github.com/sushi-shi/flowistry/pull/43); gates pending | Only experiment `ab7857370` ported; 107 core tests pass with engine-diff/shadow-eager. Bounded-scan integration, symmetric matrix equality and independent ungrouped checks added. See [recurse-row-groups.md](recurse-row-groups.md). Acceptance still depends on completed baseline gates and integrated corpus/performance evidence. |
| 4. Seed rows | Candidate prepared, [#44](https://github.com/sushi-shi/flowistry/pull/44); gates pending | Fresh-root type templates preserve the traversal cutoff, with bounded temporary storage and shadow comparison to original seeds. 108 core tests pass; two-mode small-case comparison passes, full corpus and stress measurements pending. See [seed-row-experiment.md](seed-row-experiment.md). |
| 5. HIR hashing | Investigated, [#45](https://github.com/sushi-shi/flowistry/pull/45); no compiler change retained | Three normal-release profiles and pinned compiler source identify metadata-driven HIR owner hashing. Examined shortcuts do not bypass it safely; see [hir-hashing-experiment.md](hir-hashing-experiment.md). No speedup claimed. |
| 6. Edit/concurrency harness | Implemented in [#47](https://github.com/sushi-shi/flowistry/pull/47); release gates pending | 84 serial, concurrency, recovery and real-project cases pass; 47 harness regressions pass. See [edit-concurrency-validation.md](edit-concurrency-validation.md). Add versioned-publication/background cases as those features land; full corpus gates remain required. |
| 7. Shared result index | Implemented candidate; final integration gates pending | Existing caches now share a publication lock and disk budget. Compiler-derived body index, revision/generation envelopes, cancellation, source-hash checks and interrupted-write recovery pass 16 new cases, plus all 84 edit cases. See [versioned-result-index.md](versioned-result-index.md). |
| 8. Persisted summaries | Pending | Portable schema, dependency graph and invalidation. |
| 9. Project command | Pending | Streaming, priorities and bounded workers. |
| 10. Neovim background work | Pending | Continue after editor #4; foreground priority and compatible pin. |
| 11. Incremental saves | Pending | Dependency-aware recomputation and rapid-save handling. |
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
   nine entries use `equivalence-remaining.{log,json}` in the engine corpus;
   Helix is the last active entry. The freed h19 slot now runs the full Recurse
   comparison of `baseline/` with `row-groups/`, in `row-groups-corpus.{log,json}`
   and `row-groups-checkpoints/`, under the same two-job/6 GiB caps.
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
   saves and safe relocation (steps 8–12). Those features remain unimplemented.
   Finish all final gates in step 13. Current continuation PR order is #41 → #42 →
   #43 → #44 → #45 → #46 → #47; nothing has been merged.
