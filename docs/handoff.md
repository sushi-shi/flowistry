# Handoff: performance program and open PR stacks

State as of 2026-09-30. Nothing from #14 onwards is merged: `master` is still `f8b5582b7` (#18).
This document says where everything is, how to verify it, in which order to land it, and
what is left to do.

## Ground rules of this fork

- **Review and merge.** Every change goes through a PR on `sushi-shi/flowistry` (the editor
  side is `sushi-shi/flowistry.nvim`). The owner reviews each PR, and PRs are then
  **squash-merged**.
- **PR descriptions** are updated before merging, with real measurements and examples.
- **Force-pushes** need the owner's go-ahead. Plain pushes of new commits are fine.
- **Evidence.** Every performance change needs identical output (a harness compare with 0
  differences) and a measurement. Anything that changes output is a separate, explicit
  decision.
- **Priority.** The backend runs in the editor loop on every save and cursor move, so even
  ~0.1 s (or a few %) is worth a PR.

## Environment

- **Toolchain.** There is no cargo, rustc or python on `PATH` outside the dev shell. Use:
  - `nix develop github:sushi-shi/flowistry` for building and testing;
  - `nix develop github:sushi-shi/flowistry#smoke` for the smoke corpus (native libraries);
  - `--refresh` if the cached flake is stale.

  The flake can't be evaluated from a git worktree path, so always use the GitHub ref.
- **Nightly `nightly-2026-05-01`.** The matching rustfmt isn't in the toolchain. Build it
  from fenix and use `rustfmt --edition 2024`:

  ```
  nix build -o /some/dir/rustfmt-tool --impure --expr 'let f = builtins.getFlake "github:nix-community/fenix/5f7e7d793cb2553410f857554de86f277ebe2f71"; in (f.packages.x86_64-linux.toolchainOf { channel = "nightly"; date = "2026-05-01"; sha256 = "ea96e87fce61b2182006a819cf8d6ac74cc7562e4bc6d60aba4107ffcba58d52"; }).rustfmt'
  ```
- **perf:** `nix build -o /some/dir/perf-tool github:NixOS/nixpkgs/1559d3daa3ecc813a650b79375ea61b6741b8746#perf`.
  - Always build tools with `-o`: `--no-link` outputs are garbage-collected within the hour.
  - `strace` doesn't work, because `ptrace_scope=2`. `perf record` and `perf stat` do.
- **Machine.** 24 cores, 31 GB of RAM, and earlyoom.
  - Run compares with `--memory-limit 6G -j 2`, and at most two heavy harnesses at a time.
  - Kill processes by **exact PID only**: a `pkill -f` pattern once killed the controlling
    shell.
  - The disk is 92% full: see [Cleanup](#cleanup).
- **Target directories.** Use one `CARGO_TARGET_DIR` per variant, under
  `target/<name>`.
- **Profiling builds.** Build with `RUSTFLAGS='-C force-frame-pointers=yes'
  CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` and record with `perf record --call-graph fp`.
  Call graphs stop at `librustc_driver.so`, which has no frame pointers, and DWARF unwinding
  doesn't help there either.

## How to measure

- **Correctness: `scripts/smoke-real-crates.py`** (see `docs/smoke-tests.md`). It runs a
  locked corpus of 25 entries (10 registry crates and 15 git projects) × 60 positions ×
  2 modes.
  - `A --compare B` diffs the canonical outputs. `--fetch` clones the git entries into a
    fresh work directory.
  - **Use the harness from #19 plus local commit `21f7f6509`.** It has
    `--memory-limit` and budgets, and compares outputs by digest. Without the digests, the
    harness itself reaches 16 GB on just once just stops running out of memory.
  - The canonical form treats range lists as sets (#28) and resolves the range table
    (#32).
- **Speed: count instructions, not wall time.** The machine is usually loaded (load
  average 15–24), and wall time then varies by ±50%.
  1. Capture the driver's command line and environment once, with a wrapper that saves
     `"$@"` and `env -0`.
  2. Replay it under `perf stat -x, -r 8 -e instructions:u,cycles:u`.
- **Per-phase timers:** set `RUST_LOG=rustc_utils::timer=info,flowistry_ide=info`. Add
  `flowistry::stats=info` for the counters, but that switches on #37's full count of
  unstable locations.
- **Positions used throughout:**

  | Position | Kind |
  |---|---|
  | just `src/error.rs 926 10` (SigOnly) | stress: 4,119 locations, 1,186 places, 63,578 argument places; out of memory on master |
  | just Recurse `src/analyzer.rs 177 4` | Recurse stress |
  | memchr `src/arch/generic/memchr.rs 283 16` | analysis-heavy |
  | serde_json `src/de.rs 462 8` | typical |

## Where the time went, and where it is now

just `error.rs 926 10`, SigOnly:

| Build | Wall | Notes |
|---|---|---|
| master | out of memory at 6 GB | |
| engine stack (#26) | 35.9 s | 168 MB of base64 output (the duplicate-output bug) |
| + #28, #29, #30, #32 | 8.4 s | |
| + #33 (demand-driven rustc) | about 2 s | rustc 2.67 s → 0.22 s |
| + #31, #36, #37, #38, #39 | about 0.7 s of analysis, plus 0.34 s of rustc | |

Remaining phases on just: rustc 0.34 s, Flow 0.20 s (0.13 s of it building the seed rows),
`compute_dependencies` 0.20 s, dependency spans 0.34 s, aliases 0.04 s, slice ranges
0.04 s.

On a **typical request** (serde_json): cargo takes about 50–75 ms, which #34 removes when
nothing cargo checks has changed. The driver takes about 110 ms, almost all of it rustc's
parsing, expansion and name resolution; our analysis takes a few milliseconds. #36 saves
another 12.6% of instructions.

## Open PRs and their stacks

`A ← B` means B's PR targets A's branch. Always use the **`origin/*`** heads:
- the local `perf/lazy-arg-rows`, `perf/place-caches`, `perf/shared-rows`,
  `perf/block-engine`, `perf/scoped-borrowck` and `perf/measure` are the engine worker's own
  versions, which still contain the dropped scoped-borrowck commit;
- the local `perf/measure-harness` is **ahead** of #19 by `21f7f6509`, which isn't pushed
  yet.

**Rebuild stack** (reimplements old #1/#4/#5 on the typed core):

| PR | Branch | Content |
|---|---|---|
| #14 | `feat/file-focus` (on master) | `file-focus`, precise focus spans with argument trimming, scoped borrowck for file-focus; tests and examples |
| #15 | on #14 | Recurse with cached callee summaries |
| #16 | on #15 | interior mutability behind shared references |
| #17 | on #16 | "maybe" slices for shared interior-mutable handles |

**Engine stack**, on master. Descriptions say "Measurements: pending" until the engine
worker reports; interim numbers are in PR comments.

| PR | Branch | Content |
|---|---|---|
| #19 | `perf/measure-harness` | harness: peak RSS, `--memory-limit`, budgets, stats. Local `21f7f6509` adds output digests (to push); budgets.tsv still has placeholder values |
| #20 | `perf/measure` | `FlowResults::stats` counters |
| #21 | `perf/fast-bitsets` | bit-set count and inclusion without clone or iteration |
| #22 | `perf/gzip-6` | gzip level 6 |
| #23 | `perf/lazy-arg-rows` | argument rows stored once per body (`LazyMatrix`, `shadow-eager` feature). Fixes just's OOM |
| #24 | `perf/place-caches` | memoized mutations and row keys |
| #25 | `perf/shared-rows` | copy-on-write rows |
| #26 | `perf/block-engine` | per-block state when exact (`batch_is_idempotent`, `engine-diff` feature). Most SigOnly bodies still fall back to the location engine |
| #29 | on #26 | forward pass: only sub-targets whose location is in the row |
| #30 | on #29 | exact linear `merge_spans` |
| #37 | on #30 | fast path for the block-engine check; hoisted location indices |
| #38 | on #37 | one global span sort for all targets |

**Output stack**, on master:

| PR | Branch | Content |
|---|---|---|
| #28 | `perf/dedup-direct-influence` | direct_influence was repeated ~195× |
| #32 | on #28 | **output format change:** a table of distinct ranges, with places referring to it by index. Needs flowistry.nvim#4 |
| #35 | on #32 | character positions via rustc's line table |
| #39 | on #35 | binary-searched direct-influence filter |

**Independent**, on master:

| PR | Content |
|---|---|
| #31 | alias relation from distinct fact pairs, plus raw-ID comparison |
| #33 | **demand-driven rustc:** the analysis runs in `after_expansion`, and closures are borrow-checked through their typeck root. Behaviour change: type errors elsewhere in the crate no longer block a request |
| #34 | cargo bypass: records the driver invocation and a snapshot of cargo's inputs, and replays when the snapshot holds; `FLOWISTRY_NO_REPLAY` disables it |
| #36 | on #33: no incremental state, and exit right after the output |

**Superseded or parked:**
- **#27** (scoped borrowck for plain `focus`, on #14): superseded by #33. Close it once #33
  lands.
- **#2** (persistent cache, on the old core): the basis for Phase 1 below.
- **Closed:** #1, #4 and #5 (replaced by #15–#17).

**flowistry.nvim**, all open:
- #1 (packaging and NixOS integration) → #2 (disk cache) → #3 (maybe tint) → **#4** (range
  table, which accepts both output formats).
- The plugin's flake pins an old backend commit. Bump the pin once the backend lands.

## Suggested merge order and restack notes

1. **#14 → #15 → #16 → #17.** #15–#17 conflict with #14's latest commit (58d38eb23, which
   moved `simple_args` into `flowistry::infoflow`).
   - The Recurse worker already rebased them: local `rebase/callee-summaries-on-14`,
     `rebase/interior-mutability-on-14` and `rebase/shared-handle-hints-on-14`.
   - Its report lists nine conflict resolutions. They are also the base of
     `perf/recurse-base`.
2. **#28 → #32 (+ nvim#4) → #35 → #39**, then **#31**. All touch `focus/mod.rs`, which
   #14 and #17 also change. When rebasing onto the merged #14–#17:
   - keep #17's `maybe_slice`, and make it use the range table;
   - `file_focus.rs` embeds `FocusOutput`, so the table format also applies per body.
3. **#33 → #36, and #34.**
   - Port the demand-driven approach to #14's `file_focus.rs` (`after_expansion`, plus
     `flowistry::mir::borrowck::body_with_borrowck_facts`), and delete `scoped_borrowck.rs`.
   - #34's `replay_request` must learn the `file-focus` command.
   - Then close #27.
4. **Engine stack #19–#26 → #29 → #30 → #37 → #38.**
   - Fill in each description from the engine worker's report.
   - **Port the changes to #14's `compute_focus_spans`**:
     - it still calls `rustc_utils`' `Span::merge_overlaps`; use `merge_spans` or
       `merge_sorted` and #38's span table;
     - it runs `compute_dependencies` forward and backward itself, so it needs #29's
       `ForwardIndex`.
   - The Recurse worker's `perf/recurse-base` shows how the engine stack sits on #15
     (`RowMatrix` trait, `written_alias_keys`, `reads_are_stable`).

Before each merge: run the full suite (`cargo test --locked -p flowistry --all-targets`,
`cargo test --locked -p flowistry_ide`, `cargo build --locked --workspace --all-targets`)
and a harness compare against the new base.

## In flight when this was written

- **Harness compares.** The PR comments will say whether they finished.
  - #33: full corpus, including the git projects. Its first run found a closure crash, now
    fixed in `34b260a32`.
  - #35 vs #32 and #37 vs #30: registry crates.
  - Results were written to the session scratchpad, which is temporary.
- **Engine worker:**
  - remaining compares, the `engine-diff` whole-corpus run, stress budgets, and timings per
    PR;
  - it keeps `21f7f6509`, and a budgets.tsv amend for #19 is to follow.
- **Recurse / Phase 0 worker.** Local branches only; nothing is pushed.
  - `perf/recurse-base` is the rebased #15–#17 plus the engine stack.
  - `perf/recurse-groups` is PR5: callee exports stored as row groups. On just Recurse
    `analyzer.rs 177 4` it cut stored rows from 20.3 M to 1.17 M and peak RSS from 2.73 to
    1.86 GB, with identical output.
  - `perf/recurse-groups-33` is the same with #33.
  - `perf/recurse-block-only` is the experiment of always using the block engine. It
    changes output in the `revisited_raw_pointer_call` fixture, where master's spurious `r`
    leaves the slice. It **needs a decision** based on the count of changed corpus runs.
  - PR9 (Recurse borrowck pre-pass) was dropped: #33 does it. PR10 (structural expansion)
    looks unnecessary, since just showed 0 group expansions.
  - Phase 0 measurements were still pending.

## What is left

1. **Seed rows for huge argument types:** 0.13 s on just's `Error::fmt`.
   - `SeedRows` computes `compute_conflicts` for each of the 63,578 argument-interior
     places, and each call re-enumerates a subtree with `interior_places`.
   - A faster exact version must reproduce `interior_places`' type-stack cut-off, since a
     subtree explored from a child can go deeper than from the root.
   - Validate it with the `shadow-eager` feature.
2. **HIR hashing, about 3% of a typical request.** rustc hashes HIR because a lib crate
   needs metadata. Compiling as `cdylib` made it much worse, so this needs a rustc option.
3. **Recurse:**
   - land PR5 (row groups);
   - decide on "block engine only";
   - Recurse budgets in budgets.tsv.
4. **Incremental analysis: `docs/incremental-analysis-plan.md`.** This is the biggest
   remaining lever: with #33, a `file-focus` run analyzes a whole file for about the
   cost of one request.
   - Phase 0 measures per-request cost, whole-project time and save latency, and its
     decision rules say which of the following to do.
   - Phase 1 rebases #2's cache onto the typed core.
   - Phase 2 is the background `project` / whole-file runs, and nvim batch mode by default.
   - Phase 3 is the save path with fingerprints.
5. **VS Code extension.** #32's change to `ide/src/focus.ts` couldn't be type-checked (no
   `node_modules`). Check it.
6. **Known harmless oddity:** on a build error both `focus` and `file-focus` exit 0 with
   no stdout. That comes from `rustc_plugin`; the editor treats empty output as a failure.

## Cleanup

- **Worktrees:** there are many under `.claude/worktrees/` and in the session scratchpad.
  Run `git worktree prune` after deleting their directories.
- **Target directories:** remove `target/plan-engine*` (about 5 GB) and
  `target/{audit-fp,all,all-fp,allbase,probe,stack-rel,buckets,spanmerge*,dedup*,rtable,charpos*,lean*,cheap*,spantable*,influence*,demand*,bypass*,alias*,fastexit,pr14*,verify-*}`
  once their PRs are merged. `target/engine/*` and `target/recurse/*` belong to the
  workers' runs.
- **Replay records:** `~/.cache/flowistry/replay/`, from #34.
- **Remote branches** of merged PRs.
