# Continuation implementation progress

The [full plan](continuation-plan.md) remains the objective. No milestone is
complete merely because tooling or a subset of its tests passes.

| Step | State | Evidence / next gate |
|---|---|---|
| 1. Combined validation | In progress, [#41](https://github.com/sushi-shi/flowistry/pull/41) | Frozen #17 and integrated builds; 18 harness/audit regressions pass. Complete corpus comparison running; independent engine/eager corpus and cache-mode/file-focus gates still pending. |
| 2. Performance baseline | Pending | Frozen release binaries available; need actual immediate-base, project and save measurements and budgets. |
| 3. Recurse row groups | Pending | Port only the relevant experiment after the baseline gates. |
| 4. Seed rows | Pending | Preserve context-sensitive traversal cutoff. |
| 5. HIR hashing | Pending | Profile and retain only proven improvements. |
| 6. Edit/concurrency harness | Pending | Existing cache regression scripts are the starting fixtures. |
| 7. Shared result index | Pending | Versioned publication and bounded persistent storage. |
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
  evidence. Recheck host process state before reusing its heavy-run slot.

The combined run uses the already-prepared corpus at
`$S/h19/target/smoke-crates`; the inherited timing uses a separate engine corpus.
Do not start another run that writes to the same prepared corpus concurrently.
Do not delete active source/build inputs or preserved experiment work.

## Next concrete actions

1. Inspect checkpoint records for differences while the full comparison runs.
   Audit its completed report with `scripts/summarize-validation.py`; investigate
   every difference/status change and missing entry.
2. Verify real cross-launch checkpoint reuse with the stable temporary-directory
   implementation in #41, using a completed small entry in a separate checkpoint
   directory after a heavy slot and its prepared source become available.
3. Run the frozen reference-check executable across the full corpus, then the
   cache-off/fresh/warm and file-focus equivalence gates. Reuse existing corpora
   serially, keeping at most two heavy harnesses active.
4. Save compact final reports in this PR, update the status table, and proceed
   through the remaining plan. #41 remains draft until its gates are satisfied.
