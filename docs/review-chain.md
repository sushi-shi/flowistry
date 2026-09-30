# Linear review guide

State: 2026-09-30. Start with #14 and follow the table. Every backend PR targets
the previous PR's branch; only #14 targets master. Nothing has been merged.
The independent stacks have been integrated, including the formerly old-core #2.

Use each PR's **Files changed** tab to review its incremental change. Existing
commit ancestry is retained through merge commits, allowing normal pushes with
no history rewriting; the commit tab consequently includes historical branches.

## Backend review order

| Step | PR | Review focus |
|---|---|---|
| 1 | [#14](https://github.com/sushi-shi/flowistry/pull/14) | File-level focus, precise argument trimming, and the editor-independent command. |
| 2 | [#15](https://github.com/sushi-shi/flowistry/pull/15) | Typed, session-scoped callee summaries for Recurse; explicit semantic precision changes. |
| 3 | [#16](https://github.com/sushi-shi/flowistry/pull/16) | Interior mutability behind shared references. |
| 4 | [#17](https://github.com/sushi-shi/flowistry/pull/17) | Separate maybe-slices for potentially shared handles. |
| 5 | [#19](https://github.com/sushi-shi/flowistry/pull/19) | Bounded-memory measurement harness, canonical output digests, and canonicalization regressions. |
| 6 | [#20](https://github.com/sushi-shi/flowistry/pull/20) | Analysis counters and phase measurement. |
| 7 | [#21](https://github.com/sushi-shi/flowistry/pull/21) | Fast bit-set count and inclusion operations. |
| 8 | [#22](https://github.com/sushi-shi/flowistry/pull/22) | Gzip level 6 for output. |
| 9 | [#23](https://github.com/sushi-shi/flowistry/pull/23) | Lazy argument rows, also used by callee summaries. |
| 10 | [#24](https://github.com/sushi-shi/flowistry/pull/24) | Place/alias caches, preserving writes through shared handles. |
| 11 | [#25](https://github.com/sushi-shi/flowistry/pull/25) | Copy-on-write shared rows. |
| 12 | [#26](https://github.com/sushi-shi/flowistry/pull/26) | Block dataflow engine with exact fallback; independent reference-engine sessions. |
| 13 | [#29](https://github.com/sushi-shi/flowistry/pull/29) | Forward dependency buckets, including the focus-span path. |
| 14 | [#30](https://github.com/sushi-shi/flowistry/pull/30) | Span merging, including the focus-span path. |
| 15 | [#37](https://github.com/sushi-shi/flowistry/pull/37) | Bounded instability checks and write fast paths that retain shared-handle effects. |
| 16 | [#38](https://github.com/sushi-shi/flowistry/pull/38) | One sorted span table for dependency queries, including precise focus spans. |
| 17 | [#28](https://github.com/sushi-shi/flowistry/pull/28) | Deduplicate direct-influence ranges without losing callee-summary behavior. |
| 18 | [#32](https://github.com/sushi-shi/flowistry/pull/32) | Indexed wire-format range table; maybe-slices and file-focus use it too. |
| 19 | [#35](https://github.com/sushi-shi/flowistry/pull/35) | Fast character-position conversion for focus and file-focus. |
| 20 | [#39](https://github.com/sushi-shi/flowistry/pull/39) | Filter unrelated direct-influence candidates before span-tree queries. |
| 21 | [#31](https://github.com/sushi-shi/flowistry/pull/31) | Faster region-alias relations. |
| 22 | [#33](https://github.com/sushi-shi/flowistry/pull/33) | Demand-driven compiler queries for focus and file-focus; remove scoped borrowck. |
| 23 | [#36](https://github.com/sushi-shi/flowistry/pull/36) | Return successful results before compiler teardown, also for file-focus. |
| 24 | [#34](https://github.com/sushi-shi/flowistry/pull/34) | Replay the compiler command when Cargo inputs are unchanged; preserve failure status. |
| 25 | [#2](https://github.com/sushi-shi/flowistry/pull/2) | Persistent semantic and snapshot caches ported onto the typed core and range-table protocol. |
| 26 | [#40](https://github.com/sushi-shi/flowistry/pull/40) | This review map, current handoff, and incremental-analysis plan. |

**Excluded:** [#27](https://github.com/sushi-shi/flowistry/pull/27) was closed as superseded by #33 with the owner’s approval. It is excluded from the
review sequence; its branch and original discussion are retained. Closed #1/#4/#5 are historical predecessors of #15–#17.

## Neovim migration and historical editor reviews

The Neovim plugin now lives under `nvim/` in this repository.
[Migration PR #53](https://github.com/sushi-shi/flowistry/pull/53) follows backend
#52 and imports the complete editor history through `e2394b5`.
All subsequent backend/editor changes use this one PR chain and root flake.
See [the migration guide](neovim-monorepo.md).

The old editor PRs are closed as superseded and remain useful historical reviews:

1. [nvim#1](https://github.com/sushi-shi/flowistry.nvim/pull/1): packaging and Nix integration.
2. [nvim#2](https://github.com/sushi-shi/flowistry.nvim/pull/2): persistent-cache UI and lifecycle.
3. [nvim#3](https://github.com/sushi-shi/flowistry.nvim/pull/3): maybe-slice tint.
4. [nvim#4](https://github.com/sushi-shi/flowistry.nvim/pull/4): inline/indexed range decoders.
5. [nvim#5](https://github.com/sushi-shi/flowistry.nvim/pull/5): comments, parameter types and formatted saves.

The imported source includes all five. Their old independent backend pins are
removed; the root package binds `plugin` and `backend` from the same source tree.
The backend range-table change (#32) and source-selection metadata (#52) can be
reviewed alongside their consumers in `nvim/`.

## Validation performed during integration

- Every backend stage #14 through #34: `cargo check --locked --workspace --all-targets`.
- Integrated backend, with and without the persistent-cache port:
  `cargo test --locked --workspace --all-targets` passed.
- Core at #34: `cargo test --locked -p flowistry --all-targets --features engine-diff,shadow-eager` passed.
- Harness canonicalization: five regressions passed.
- Persistent cache: 51 cross-process cases plus selected-body compilation-error rejection passed.
- Snapshot cache: 31 cases passed; 17 hits invoked no compiler (17–68 ms in this test, not a corpus benchmark).
- VS Code: locked dependency installation and `npm run tc` passed.
- Neovim with the integrated release backend: 204 frontend, 101 callee-summary,
  and 45 persistent-cache assertions passed.

The digest harness inherited from Claude accidentally compared duplicate range
lists and unresolved range-table indexes. #19 now restores set canonicalization
and resolves indexes before hashing, including tables serialized after places.
Previous #35/#37 difference counts from that harness are not valid semantic evidence.

Corrected #35 vs #32 on memchr and serde_json: **230 successful pairs, zero output
differences**, plus 10 paired benign failures (240 pairs total). Corrected #37 vs #30 across all ten registry crates: **1,158 successful pairs,
zero output differences**, plus 42 paired benign failures (1,200 pairs total). #33's earlier full-corpus run had
2,811 successful pairs with zero output differences and two OOM-to-success status
changes. These comparisons used the original individual optimization branches,
not a full before/after corpus audit of every restacked PR.

**Before merging:** finish the outstanding corpus comparisons and measurements
against the new immediate bases. Semantic changes in #14–#17 are intentional and
need their own review. Build/test success does not establish unchanged corpus
output or a speedup for every optimization. Existing PR measurements describe
the original branches unless explicitly dated as an integrated rerun.

## Merging later

The owner's established workflow is to review and then squash-merge one PR at a
time. Squashing changes ancestry: do not bulk-merge the remaining stack or assume
GitHub's automatic base retargeting makes the next diff correct. After each squash,
restack the remaining PRs onto the resulting master commit and verify the next
PR's three-dot diff before proceeding. A rewrite/force-push still needs the
owner's explicit go-ahead; this preparation used only normal pushes.

## Unsubmitted optimization work

Local Recurse row-group experiments, seed-row construction, HIR hashing, Phase 0
measurements, and future background/project analysis are described in
[the handoff](handoff.md). They have not been silently included in these PRs.

The next work is specified in the [feature-by-feature continuation plan](continuation-plan.md).
It extends this chain with validation, the remaining optimization experiments,
and background/project analysis plus incremental save handling.

## Implementation continuation

The chain now continues after #40 with [#41](https://github.com/sushi-shi/flowistry/pull/41),
resumable corpus validation and exact coverage auditing (draft while full gates run).
Then [#42](https://github.com/sushi-shi/flowistry/pull/42) adds interleaved performance
samples, isolated replay/warmup controls and distribution reports (draft until
the baseline measurements and budgets are complete).
Next is [#43](https://github.com/sushi-shi/flowistry/pull/43), the Recurse row-group
port with integration/reference fixes; its full corpus and performance gates are
also pending.
Then [#44](https://github.com/sushi-shi/flowistry/pull/44) tests fresh-root type
templates for seed construction. Its semantic regressions pass; retention still
depends on full corpus and performance evidence.
[#45](https://github.com/sushi-shi/flowistry/pull/45) records the HIR hashing
investigation and rejected compiler shortcuts; it retains no compiler change.
[#46](https://github.com/sushi-shi/flowistry/pull/46) preserves compiler signal
termination through replay/snapshot wrappers and records the reference OOM triage.
[#47](https://github.com/sushi-shi/flowistry/pull/47) adds compiler/body-work tracing
and the edit/concurrency oracle: all 84 current cases pass in both modes, including
isolated real-project edits. Versioned publication/background cases and full
release gates remain pending.
[#48](https://github.com/sushi-shi/flowistry/pull/48) extends the existing caches
with a shared body index, revision/generation publication checks and a combined
disk budget. Its 16 new publication cases and all 84 edit cases pass; project and
editor integration remain later features.
[#49](https://github.com/sushi-shi/flowistry/pull/49) persists portable callee
summaries using shared semantic fingerprints and dependency snapshots with reverse
edges. Its 18 summary, 84 edit and 16 publication cases pass. The full Recurse
summary/output gate now passes all 1,440 selections and 18,008 structural summary
checks; final-tip and performance gates remain pending.
[#50](https://github.com/sushi-shi/flowistry/pull/50) adds explicit filename mappings
for file-focus and portable file identities in the result index. The foreign-macro
regression and all 124 process-level validation cases pass; corpus gates continue.
[#51](https://github.com/sushi-shi/flowistry/pull/51) adds the project worker
foundation: explicit Cargo targets, portable inventories, stable body selection,
shared-index filling and serialized Cargo launchers. Its 137 process cases pass;
the public coordinator, streaming, priorities, cancellation and resource limits
remain the next feature-9 increment.
[#52](https://github.com/sushi-shi/flowistry/pull/52) follows #51 with the reported
constructor highlight refinement and compiler-derived comment/type selection
metadata. Review it with [nvim#5](https://github.com/sushi-shi/flowistry.nvim/pull/5).
See
[source-selection.md](source-selection.md) for validation and conservative limits.
[#53](https://github.com/sushi-shi/flowistry/pull/53) imports the editor into
`nvim/` and binds both packages to one source tree. Its paired-package CI passes.
[#54](https://github.com/sushi-shi/flowistry/pull/54) follows #53 with the public
project coordinator: streaming outcomes, cursor/file/body priorities, per-body
Linux/systemd memory/time bounds, resumable reuse, and descendant cancellation.
Its 16 coordinator and 13 worker scenarios pass, alongside 38 IDE tests and 55
Python regressions. Full cache corpus runs and large-project measurements remain
acceptance gates; see [the coordinator evidence](project-coordinator.md).
Track all thirteen remaining milestones in [continuation-progress.md](continuation-progress.md).
