# Handoff: integrated review chain and optimization work

Updated 2026-09-30 after taking over the latest Claude optimization session.
The Neovim plugin has since moved into `nvim/`; see
[the migration guide](neovim-monorepo.md) for the shared package and review chain.
The latest continuation is [#59](https://github.com/sushi-shi/flowistry/pull/59),
`feat/safe-layout-reuse` in `/tmp/flowistry-layout-reuse`, based directly on #58.
Its final runtime is frozen at `3ddebc5e3` as `layout-reuse-v3`; focused tests and
all five paired Nix checks pass. Eligible layout saves now relocate without
compiler startup; source-sensitive cases fall back. #58's compiler-state
experiment remains rejected on measured instruction regressions.

Both coordinator-era full cache corpus runs passed all 2,880 comparisons each.
Two new full runs now validate the layout candidate using separate engine/h19
prepared roots; see [current progress](continuation-progress.md) for exact
artifacts and process observations. Earlier process/worktree descriptions below
are historical. The remaining work includes final/reference/resource gates,
quiet performance measurements and actual budgets; the complete plan is active.

Master remains `f8b5582b7` (#18); nothing was merged. The canonical review order
is [docs/review-chain.md](review-chain.md), also linked from every active PR.

## Source conversation

Claude session `447566e4-c0fd-4024-8821-02de994cb283` ran from September 28
through September 30. Its last handoff was backend #40, commit `89211ad80`.
Local transcript:

`/home/sheep/.claude/projects/-home-sheep-Projects-flowistry/447566e4-c0fd-4024-8821-02de994cb283.jsonl`

The user's priorities were to exhaust worthwhile optimizations before joint PR
review; even roughly 0.1 seconds matters in the editor loop. The latest request
was to make all existing work linearly reviewable. The original detailed handoff
remains available in git at `89211ad80:docs/handoff.md`.

## Completed in this takeover

- Integrated the feature, engine, output, and compiler-startup stacks into one
  backend chain without force-pushing. Each PR retains its original number.
- Preserved typed callee summaries, interior mutability, shared-handle effects,
  precise focus spans, and indexed maybe-slices through the conflict resolutions.
- Applied demand-driven queries and early output to file-focus; removed obsolete
  scoped borrowck. #27 was closed as superseded by #33 with the owner’s approval.
- Ported #2 onto the full chain, with schema-2 portable range tables, cache integrity
  checks, and compatible Cargo/snapshot replay. This completes the implementation
  of the Phase 1 port; the Phase 0 decision/performance work remains pending.
- Corrected digest canonicalization in #19 and in the generated harness used by
  the inherited benchmark queue. Validated compiler, cache, editor, and reference
  engine behavior; see the review guide for exact checks and scope.

## Workspaces and environment

The original user checkout remains on `feat/cached-callee-summaries`. Integrated
branches are named `review/prN` locally and pushed to the existing PR branches.
The original Neovim checkout was also left on its existing branch.

| Path | Purpose |
|---|---|
| `/tmp/flowistry-review-chain` | Backend through #34 |
| `/tmp/flowistry-cache-port` | Backend through #2 |
| `/tmp/flowistry-review-docs` | #40 and this guide |
| `/tmp/flowistry-nvim-review` | Editor #4 |
| `/tmp/flowistry-stage-check` | Detached per-PR build checks |
| `/tmp/flowistry-review-test` | Detached integration/TypeScript checks |
| `/tmp/flowistry-smoke-canonical` | Corrected harness checkout |

Claude scratchpad (called `S` below):

`/tmp/claude-1000/-home-sheep-Projects-flowistry/447566e4-c0fd-4024-8821-02de994cb283/scratchpad`

Use `$S/engine/dev COMMAND...` for the cached Nix smoke environment, including
Cargo, nightly-2026-05-01 rustc, Python, native libraries, SYSROOT and library paths.
The matching formatter is `$S/engine/rustfmt-root/bin/rustfmt --edition 2024`.
Otherwise use `nix develop github:sushi-shi/flowistry#smoke`; local git-worktree
flake evaluation was not reliable in the inherited setup. Keep Nix outputs linked
with `-o` so garbage collection does not remove them.

The host has 24 cores and 31 GB RAM. Use `--memory-limit 6G -j 2` for corpus
compares, at most two heavy harnesses concurrently. Use exact PIDs if a process
needs stopping. Do not broadly kill by command-line pattern. Disk pressure prompted a targeted cleanup after validation: 87.6 GiB was freed
from twelve obsolete debug/test builds. See the cleanup record below.

## Inherited processes and remaining validation

The inherited jobs are useful and have been left running. At inspection:

- PID 1221886: `$S/engine/pipeline3`, finished stress runs and waiting for a
  quiet machine before the first timing pair (master/measure). Its timing helper
  intentionally waits for two quiet checks without Cargo/rustc/corpus jobs.
- PID 1478783: `pipeline4`, waiting for pipeline3, then rerunning the three
  engine comparisons killed by earlyoom.
- PID 3549564: `pipeline5`, waiting for pipelines3/4, then rerunning the killed
  block-engine reference comparison across the corpus.

Their logs and JSON reports are under `$S/engine/results`; pipeline logs are
`$S/engine/pipeline{3,4,5}.log`. PIDs are a dated observation; recheck before acting.
The generated `target/engine/harness/scripts/smoke-real-crates.py` has the corrected
canonicalizer, with all five regression tests passing, for future queued runs.

Corrected #37 registry compare completed: 1,158 successful pairs, zero output
differences, and 42 paired benign failures. Report: `/tmp/flowistry-cheap-corrected.{log,json}`.
Corrected #35 two-crate compare: `/tmp/flowistry-charpos-quick.{log,json}`.
A complete corrected #35 registry rerun remains pending. Do not interpret old
duplicate/index differences as real semantic changes. Do not discard status
changes when reporting comparisons.

Other takeover logs are `/tmp/flowistry-{stage-check,review-test,review-shadow,
cache-workspace,cache-semantic,cache-snapshot,ide-tc,nvim-frontend,nvim-summaries,
nvim-cache}.log`. These paths are temporary; copy final evidence into a durable
report before cleanup. Tests and commands themselves are committed.

## Remaining optimization work

The owner has requested all three remaining areas. Follow the
[feature-by-feature continuation plan](continuation-plan.md) for implementation
order, dependencies and completion gates.

Historical measurements from the original branches (not fresh timings of this
combined chain) reduced just `src/error.rs 926 10`, SigOnly, from master OOM at
6 GB to about 0.7 seconds of analysis plus 0.34 seconds of rustc. Typical
serde_json requests spent roughly 110 ms in the driver, primarily rustc parsing,
expansion and name resolution. Cargo replay removed another 50–75 ms.

1. Seed rows: about 0.13 seconds on just's Error::fmt, repeatedly exploring
   interior-place subtrees. Preserve the type-stack cutoff and validate with
   `shadow-eager`.
2. HIR hashing: about 3% of a typical request. The cdylib experiment was slower.
3. Recurse experiments are local and unsubmitted: `perf/recurse-base`,
   `perf/recurse-groups`, `perf/recurse-groups-33`, `perf/recurse-block-only`.
   Row groups reportedly cut stored rows from 20.3M to 1.17M and peak RSS from
   2.73 to 1.86 GB on just analyzer.rs 177 4. Revalidate on this chain before PRs.
   Block-only changes the revisited_raw_pointer_call fixture; that semantic
   change needs explicit review. No live Recurse worker was found.
4. Finish per-PR measurements, the engine reference corpus, and real budgets
   (the committed budgets.tsv still contains placeholders).
5. Follow [the incremental plan](incremental-analysis-plan.md): finish Phase 0
   measurements, then decide on project/background analysis and save-path work.

Use instruction counts for comparable performance evidence on this loaded host;
wall time has been noisy. Preserve source revisions, statuses, corpus selection,
mode, memory cap, and output equivalence in each report. Benchmark default cache
behavior separately from cold analysis so warm hits do not masquerade as solver
speedups.

## Review and cleanup rules

The owner reviews before squash merges. Ordinary pushes are authorized; force
pushes require an explicit go-ahead. Do not merge while preparing the review.
The guide explains why the remaining stack needs attention after each squash.

After final reports are durable and their PRs have landed, unused target variants,
worktrees, and replay records can be removed. Do not remove active pipeline
inputs or unsubmitted Recurse work. Cleanup on the owner’s request removed only the `debug` subdirectory of
`target/{alias-test,bypass-test,charpos-test,dedup-test,demand-test,influence-test,
lean-test,spantable-test,cheap-test,pr14,verify-stack,core-rewrite}` after checking
that no live process had an executable, mapping, open file, working directory, or
Cargo target there. These are rebuildable artifacts from old test runs.

87.6 GiB was reclaimed. All release variants, active pipeline inputs, corpus
caches, `target/engine`, `target/recurse`, current `review-chain`/`review-cache`
builds, source worktrees and results were retained. The exact local deletion
manifest is `target/cleanup-2026-09-30.json`.

## Implementation has started

The new chain starts at [#41](https://github.com/sushi-shi/flowistry/pull/41). See
[continuation-progress.md](continuation-progress.md) for authoritative milestone
status, frozen builds, active corpus paths and the inherited queue transition.
The earlier process inventory above is historical; controllers 1221886, 1478783
and 3549564 were retired when combined-chain validation began.
