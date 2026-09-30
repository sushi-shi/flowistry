# Plan: background project analysis and fast incremental re-analysis

Status 2026-09-30: the Phase 1 persistent-cache port is implemented in backend #2
at the end of the [review chain](review-chain.md), with cross-process and editor
validation passing. Phase 0 measurements and later background/project work remain
pending. The plan below retains the design and decision criteria.

The [feature-by-feature continuation plan](continuation-plan.md) is now the
execution order. The design below is background; its original deferral gates no
longer cancel the requested background/save work. Measurements determine the
implementation and rollout. No merge of the existing stack is needed to begin.

## Goals

1. **No toggle.** Analyse the whole project in the background at startup, so a
   function's results are ready before the cursor gets there. Analysis should
   follow the user without per-function enabling.
2. **Fast re-analysis on save. This matters most.** When a file changes,
   re-analyse only what changed.
3. **Reuse and skip.** Reuse previous results to compute new ones faster, and
   skip analysis entirely when nothing semantically changed.
4. **Pins across functions (later).** A pinned value's highlight follows it into
   callees and callers.
5. **Never serve stale results.** Every reuse path is checked against a
   from-scratch analysis across the example projects, under many kinds of edits.

## What exists today

| Piece | Where | State |
|---|---|---|
| One compiler process on an analysis miss | integrated backend | snapshot hits skip startup; current combined-chain latency and memory baselines are pending |
| `file-focus`: a file's bodies in one compiler session | #14 (review) | nvim batch mode is optional, off by default, and limited to files of at most 600 lines |
| Callee summaries shared per session | #15 (review) | per process only |
| Per-function persistent cache: keys include the backend, compiler options, crate metadata, declarations, the body's MIR, region topology, borrow facts, HIR mapping, and in Recurse its transitive local callees; portable token-relative ranges | integrated #2 | schema-2 indexed portable ranges; cross-process and editor tests pass; combined performance measurements pending |
| Snapshot replay: a saved-input fingerprint answers before cargo/rustc start | integrated #2 | validated snapshots cover source/build inputs; broader modification-matrix tests remain planned |
| Editor-side retention and pin tracking across edits | flowistry.nvim | works per function |
| Locked corpus plus smoke/perf harness (`--compare --phases --repeat`) | #10 | the base for the tests below |

## Architecture

### A. Result store

The persistent cache from #2 is now ported onto the typed core. Extend it into the
shared result store; do not introduce competing invalidation rules.

- **Background analysis fills it; editor requests read it.** A hit costs no compile.
- **Keys stay compiler-validated,** as in #2.
- **Persisted callee summaries are planned.** Audit all fields and define an
  explicitly portable schema; a missing lifetime alone does not establish
  portability. Key summaries by the callee's validated semantic inputs and
  dependencies, preserving recursive-component and fallback behavior.

### B. `project` command: the whole crate in one session

`cargo flowistry project [--package P] [--priority FILE:LINE ...]`

- Analyses every body of the crate in one compiler session: bodies at the
  priority positions first, then the rest of their files, then everything else.
- Streams one record per body, and writes each result into the store as soon as
  it is done.
- Release Flowistry-owned per-body results promptly. Compiler arenas and query
  caches may retain borrow facts until process exit, so use bounded worker batches
  if measurements require it; do not assume all per-body memory is reclaimable.
- Workspaces run one process per package, in parallel and at low priority
  (`nice`), and can be cancelled.

### C. Save path: re-analyse only what changed

Each tier is tried in order. The first that applies answers.

1. **Proven-safe layout edits:** relocate existing ranges by token index without
   compilation only where all observable semantic inputs are proven unchanged.
   Macros, source includes, doc comments and build scripts require conservative
   fallback; see feature 12 of the continuation plan.
2. **Build inputs unchanged since the last validated run:** the snapshot replays
   the stored response. No compile.
3. **Otherwise:**
   - Run an incremental compile. rustc's incremental cache keeps type checking
     cheap, but borrow facts are recomputed, because the backend overrides
     `mir_borrowck`.
   - Fingerprint every body, and analyse only those whose fingerprint changed.
   - In Recurse mode, also re-analyse bodies whose transitive callees changed;
     the callee dependency graph is kept in the store.
   - Reuse persisted summaries of unchanged callees.
   - Order: the function under the cursor first, then the rest of the saved file,
     then dependents in other files.
4. **Later, research:** warm-start a changed body's dataflow from its previous
   fixed point. It only pays off if the fixed-point loop itself dominates, which
   is unknown; measure first.

### D. Editor (flowistry.nvim)

- **Startup:** one background `project` run with the open buffers as priorities.
  A progress indicator in the statusline. Results come from the store, and batch
  mode becomes the default.
- **On save:** request the saved file, current function first. Old highlights
  stay, marked stale, until the new result arrives.
- **Remove the per-function toggle:** analysis is always on, with a global switch
  only.
- **Pin propagation (later):**
  - When a pinned place flows into a call argument, the callee's slice for the
    matching parameter can be shown.
  - `CallSite`/`EffectPath` already map arguments to parameters.
  - The protocol needs a "place in another body" reference.

### E. Long-running backend (later, only if Phase 0 says startup dominates)

A daemon that keeps cargo metadata and the process warm. A rustc session can't be
reused across edits, so its payoff is limited to startup cost. Measure before
building it.

## Gate: Phase 0 measurements (after the engine perf work)

On the locked corpus (just, tokei, alacritty, niri, helix, bevy_ecs plus the
registry crates), with release builds:

1. **Per-request cost** at typical positions: cargo and process startup, the
   rustc frontend (warm incremental), borrowck facts, analysis, IDE and output.
2. **Whole-project time and peak memory** per project: `file-focus` over every
   file.
3. **Save latency:** edit a function body, then run `file-focus` on the file,
   warm.

Decision rules:

- **Whole-project analysis takes minutes, or save latency is above about 300 ms:**
  Phases 1–3 are important. Do them.
- **Save latency is already below about 300 ms without caching:** keep Phase 1,
  the cache, for instant reopen and unchanged files. Defer Phase 3.
- **Startup dominates save latency:** consider E.

## Phases (each a PR or stack, measured against the previous one)

| Phase | Content | Needs |
|---|---|---|
| 0 | Measurements above; decide | engine perf merged |
| 1 | Rebase the #2 cache onto the typed core (function cache, snapshot replay, response cache) plus the modification-matrix tests | — |
| 2 | `project` command, streaming, priorities, bounded memory; nvim background run, batch default, no toggle | 1, engine fixes |
| 3 | Save path: fingerprint diffing, Recurse dependency invalidation, persisted summaries | 1, 2 |
| 4 | Pin propagation, and a daemon or warm-start only if measurements justify them | 3 |

## Testing: the modification matrix

A harness extending `scripts/smoke-real-crates.py`. For each corpus project, it
applies scripted edits to a pinned checkout. For each edit it runs the
incremental/cached path and a from-scratch analysis. It asserts that the
canonicalized outputs are equal for every body in the edited file and its
dependents, and records latency.

The edits:

| Group | Edits |
|---|---|
| Layout only | whitespace-only; comment added or removed; rustfmt reflow |
| Inside a body | local variable renamed; literal changed; statement added or removed; control flow changed |
| Other bodies | callee's body changed (Recurse must invalidate its callers); callee added or removed |
| Signatures and types | signature changed; struct field added, removed or reordered; enum variant added; trait impl changed |
| Items and files | function moved within its file or to another file; new module file; file deleted |
| Build configuration | `cfg`/feature flip; dependency version bump (lockfile); build-script input changed; environment variable read by `env!` |
| Broken and reverted code | an edit that makes the code fail to compile, then reverted; undo to an earlier content (the cache must recover the old entry) |

Plus concurrency: two saves in quick succession, and an editor request while a
background run is in progress.

Pass criteria:
- No stale results.
- No crashes.
- Latency per tier recorded.
- Proven-safe layout cases do not compile; unsupported/source-sensitive cases
  invalidate or fall back to compiler validation.
- Validated unchanged bodies do not rerun the solver unless their dependencies
  or analysis context changed. Compiler validation can still be necessary.

## Risks

- **Serving stale results is the worst possible bug.** Mitigations: strict
  compiler-validated keys, the matrix above, and `FLOWISTRY_CACHE=off|refresh`
  escape hatches.
- **Memory of whole-project runs.** They depend on the engine fixes (lazy places,
  block-level states) and on releasing per-body state.
- **The rustc_plugin cache interplay.** The plugin forces a re-check of the target
  crate. The borrowck override defeats incremental caching of borrow facts, so
  measure its share.
- **Two repos.** The editor work lives in flowistry.nvim; its pin must be updated
  per backend release.
